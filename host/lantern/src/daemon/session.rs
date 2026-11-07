use std::time::Duration;

use anyhow::{bail, Context, Result};
use lantern_proto::{read_frame_async, write_frame_async, Control, Frame, Kind};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};

use crate::adb::Adb;

const HELPER_JAR: &[u8] = include_bytes!("../../assets/helper.jar");

pub struct SessionConn {
    pub tx: mpsc::Sender<Frame>,
    pub rx: mpsc::Receiver<Frame>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    child: Option<Child>,
    adb: Adb,
    forward: Option<u16>,
    helper_sha: String,
}

impl SessionConn {
    pub fn serial(&self) -> &str {
        self.adb.serial()
    }

    pub fn helper_sha(&self) -> &str {
        &self.helper_sha
    }

    /// Closes both socket halves before waiting for the helper. A jammed peer therefore cannot
    /// hold teardown open; the local adb child is killed if remote cleanup exceeds the bound.
    pub async fn close(mut self) {
        let bye = Frame {
            kind: Kind::Control,
            stream: 0,
            payload: Control::Bye.encode(),
        };
        let _ = timeout(Duration::from_millis(250), self.tx.send(bye)).await;

        if let Some(reader) = self.reader.take() {
            reader.abort();
            let _ = timeout(Duration::from_secs(1), reader).await;
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
            let _ = timeout(Duration::from_secs(1), writer).await;
        }

        if let Some(port) = self.forward.take() {
            if let Err(error) = self.adb.remove_forward(port) {
                tracing::debug!(%error, port, "forward already removed");
            }
        }
        if let Some(mut child) = self.child.take() {
            if timeout(Duration::from_secs(3), child.wait()).await.is_err() {
                tracing::debug!("helper did not exit after socket close; killing adb child");
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(1), child.wait()).await;
            }
        }
    }
}

impl Drop for SessionConn {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        if let Some(port) = self.forward.take() {
            let _ = self.adb.remove_forward(port);
        }
    }
}

struct PendingSession {
    adb: Adb,
    child: Option<Child>,
    forward: Option<u16>,
}

impl Drop for PendingSession {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        if let Some(port) = self.forward.take() {
            let _ = self.adb.remove_forward(port);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn helper_command(remote: &str, name: &str, token: &str) -> String {
    format!("CLASSPATH={remote} exec app_process / dev.lantern.helper.Main serve {name} {token}")
}

pub async fn start(adb: &Adb) -> Result<SessionConn> {
    let helper_sha = hex(&Sha256::digest(HELPER_JAR));
    let remote = format!("/data/local/tmp/lantern-helper-{helper_sha}.jar");
    let installed = adb
        .shell(&["sha256sum", &remote])
        .map(|output| output.split_whitespace().next() == Some(helper_sha.as_str()))
        .unwrap_or(false);
    if !installed {
        let host = std::env::temp_dir().join(format!("lantern-helper-{helper_sha}.jar"));
        std::fs::write(&host, HELPER_JAR).context("write embedded helper jar")?;
        let pushed = adb.push(&host.to_string_lossy(), &remote);
        let _ = std::fs::remove_file(&host);
        pushed?;
    }

    let mut token_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut token_bytes);
    let token = hex(&token_bytes);
    let name = format!("lantern-{}", &token[..8]);
    let command = helper_command(&remote, &name, &token);
    let child = adb.spawn_shell(&command)?;
    let mut pending = PendingSession {
        adb: adb.clone(),
        child: Some(child),
        forward: None,
    };

    let stdout = pending
        .child
        .as_mut()
        .unwrap()
        .stdout
        .take()
        .context("helper stdout")?;
    let mut stdout = BufReader::new(stdout).lines();
    let line = timeout(Duration::from_secs(5), stdout.next_line())
        .await
        .context("timed out waiting for helper listener")?
        .context("read helper listener")?
        .context("helper exited before listening")?;
    if line.trim() != format!("listening {name}") {
        bail!("unexpected helper startup output: {line:?}");
    }
    tokio::spawn(async move {
        while let Ok(Some(line)) = stdout.next_line().await {
            tracing::debug!(target: "lantern::helper", "{line}");
        }
    });

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("allocate local port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    adb.forward(port, &name)?;
    pending.forward = Some(port);

    let deadline = Instant::now() + Duration::from_secs(3);
    let stream = loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => break stream,
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, port, "helper not accepting yet");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error).context("connect to helper for 3 s"),
        }
    };
    let stream = handshake(stream, &token, 1).await?;
    let _ = stream.set_nodelay(true);

    let (tx, mut outbound) = mpsc::channel::<Frame>(64);
    let (inbound, rx) = mpsc::channel::<Frame>(64);
    let (mut read_half, mut write_half) = stream.into_split();
    let reader = tokio::spawn(async move {
        loop {
            match read_frame_async(&mut read_half).await {
                Ok(Some(frame)) => {
                    if inbound.send(frame).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(%error, "helper read ended");
                    break;
                }
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(frame) = outbound.recv().await {
            if let Err(error) = write_frame_async(&mut write_half, &frame).await {
                tracing::debug!(%error, "helper write ended");
                break;
            }
        }
    });

    pending.forward.take();
    Ok(SessionConn {
        tx,
        rx,
        reader: Some(reader),
        writer: Some(writer),
        child: pending.child.take(),
        adb: adb.clone(),
        forward: Some(port),
        helper_sha,
    })
}

pub async fn handshake(mut stream: TcpStream, token: &str, version: u32) -> Result<TcpStream> {
    let hello = Control::Hello {
        version,
        token: token.to_owned(),
    };
    write_frame_async(
        &mut stream,
        &Frame {
            kind: Kind::Control,
            stream: 0,
            payload: hello.encode(),
        },
    )
    .await
    .context("send hello")?;

    let reply = timeout(Duration::from_secs(3), read_frame_async(&mut stream))
        .await
        .context("timed out waiting for hello reply")?
        .context("read hello reply")?
        .context("connection closed before hello reply")?;
    if reply.kind != Kind::Control || reply.stream != 0 {
        bail!("expected hello reply in a control frame on stream 0");
    }
    match Control::decode(&reply.payload).context("decode hello reply")? {
        Control::Hello { version: 1, token } if token.is_empty() => Ok(stream),
        Control::Hello { version, .. } => bail!("helper protocol version mismatch: {version}"),
        Control::Error { msg } => bail!("helper rejected handshake: {msg}"),
        other => bail!("expected hello reply, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::helper_command;

    #[test]
    fn helper_command_sets_classpath_and_uses_app_process_root() {
        assert_eq!(
            helper_command("/data/local/tmp/helper.jar", "lantern-12345678", "abcd"),
            "CLASSPATH=/data/local/tmp/helper.jar exec app_process / dev.lantern.helper.Main serve lantern-12345678 abcd"
        );
    }
}
