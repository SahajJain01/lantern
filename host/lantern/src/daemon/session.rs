use std::time::Duration;

use anyhow::{bail, Context, Result};
use lantern_proto::{read_frame_async, write_frame_async, Control, Frame, Kind};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdout};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};

use crate::adb::Adb;

const HELPER_JAR: &[u8] = include_bytes!("../../assets/helper.jar");

/// The only protocol version this daemon speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Why a live session ended on its own (as opposed to being closed by the daemon).
pub type EndReason = &'static str;

/// What [`SessionConn::next`] yields.
pub enum Inbound {
    Frame(Frame),
    Ended(EndReason),
}

pub struct SessionConn {
    pub tx: mpsc::Sender<Frame>,
    pub rx: mpsc::Receiver<Frame>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    output: Option<JoinHandle<()>>,
    done: mpsc::Receiver<EndReason>,
    child: Option<Child>,
    adb: Adb,
    forward: Option<u16>,
    helper_sha: String,
}

impl SessionConn {
    /// Wraps an already-handshaken helper connection with no adb resources attached. Used by the
    /// in-process fake-helper tests; `start` uses the same path with its child and forward.
    pub fn attach(stream: TcpStream, adb: Adb, helper_sha: impl Into<String>) -> Self {
        Self::from_parts(stream, adb, None, None, None, helper_sha.into())
    }

    fn from_parts(
        stream: TcpStream,
        adb: Adb,
        child: Option<Child>,
        helper_output: Option<Lines<BufReader<ChildStdout>>>,
        forward: Option<u16>,
        helper_sha: String,
    ) -> Self {
        let _ = stream.set_nodelay(true);
        let (done_tx, done) = mpsc::channel::<EndReason>(4);
        let (tx, mut outbound) = mpsc::channel::<Frame>(64);
        let (inbound, rx) = mpsc::channel::<Frame>(64);
        let (mut read_half, mut write_half) = stream.into_split();

        let reader_done = done_tx.clone();
        let reader = tokio::spawn(async move {
            let reason = loop {
                match read_frame_async(&mut read_half).await {
                    Ok(Some(frame)) => {
                        if inbound.send(frame).await.is_err() {
                            break "session receiver dropped";
                        }
                    }
                    Ok(None) => break "helper closed the connection",
                    Err(error) => {
                        tracing::debug!(%error, "helper read ended");
                        break "helper connection failed";
                    }
                }
            };
            let _ = reader_done.try_send(reason);
        });
        let writer = tokio::spawn(async move {
            while let Some(frame) = outbound.recv().await {
                if let Err(error) = write_frame_async(&mut write_half, &frame).await {
                    tracing::debug!(%error, "helper write ended");
                    break;
                }
            }
        });
        // The adb shell's stdout reaches EOF exactly when the helper process (or the adb
        // transport carrying it) has gone, so it doubles as the child-exit signal.
        let output = helper_output.map(|mut lines| {
            let output_done = done_tx.clone();
            tokio::spawn(async move {
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "lantern::helper", "{line}");
                }
                let _ = output_done.try_send("helper process exited");
            })
        });

        Self {
            tx,
            rx,
            reader: Some(reader),
            writer: Some(writer),
            output,
            done,
            child,
            adb,
            forward,
            helper_sha,
        }
    }

    pub fn serial(&self) -> &str {
        self.adb.serial()
    }

    pub fn helper_sha(&self) -> &str {
        &self.helper_sha
    }

    /// Resolves when the helper side ends the session: socket EOF/error or helper process exit.
    /// Cancel-safe; pending forever while the session is healthy.
    pub async fn closed(&mut self) -> EndReason {
        self.done.recv().await.unwrap_or("session tasks ended")
    }

    /// The next frame from the helper, or why the session ended. Cancel-safe. Consuming `rx`
    /// through this keeps the reader task from stalling on a full inbound queue, which would
    /// otherwise also hide socket EOF.
    pub async fn next(&mut self) -> Inbound {
        tokio::select! {
            frame = self.rx.recv() => match frame {
                Some(frame) => Inbound::Frame(frame),
                // The reader queues why on `done` before it exits (and so before `rx` closes),
                // so the reason is already there if it is coming at all: never wait for it.
                None => Inbound::Ended(self.done.try_recv().unwrap_or("helper connection ended")),
            },
            why = self.done.recv() => Inbound::Ended(why.unwrap_or("session tasks ended")),
        }
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
        if let Some(writer) = self.writer.take() {
            // Let the writer flush the Bye before the socket is torn down.
            let _ = timeout(Duration::from_millis(250), async {
                while self.tx.capacity() < self.tx.max_capacity() && !writer.is_finished() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            writer.abort();
            let _ = timeout(Duration::from_secs(1), writer).await;
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
            let _ = timeout(Duration::from_secs(1), reader).await;
        }

        if let Some(port) = self.forward.take() {
            if let Err(error) = self.adb.remove_forward(port).await {
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
        if let Some(output) = self.output.take() {
            output.abort();
        }
    }
}

impl Drop for SessionConn {
    fn drop(&mut self) {
        for task in [self.reader.take(), self.writer.take(), self.output.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        if let Some(port) = self.forward.take() {
            self.adb.remove_forward_blocking(port);
        }
    }
}

/// Owns everything a half-started session has created. Dropping it (start failed, or the start
/// future was cancelled by a signal or an Offline) kills the helper and removes the forward.
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
            self.adb.remove_forward_blocking(port);
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

/// How long a new session waits for a previous session's helper (or its Cleanup) to finish
/// restoring the phone before killing it.
pub const PREVIOUS_HELPER_WAIT: Duration = Duration::from_secs(10);
const PREVIOUS_HELPER_POLL: Duration = Duration::from_millis(200);
/// Lists every live helper process (Main or Cleanup) on the phone. The bracket keeps the
/// pattern from matching the shell running it; `true` makes "none" a success.
const HELPER_PIDS_CMD: &str = "pgrep -f 'dev[.]lantern[.]helper'; true";

async fn helper_pids(adb: &Adb) -> Result<Vec<u32>> {
    let out = adb
        .shell(&[HELPER_PIDS_CMD])
        .await
        .context("list previous helper processes")?;
    Ok(out
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect())
}

/// Waits (up to `bound`) until no helper process of an earlier session is alive on the phone,
/// then kills any that remain: they are ours, and a helper's `Session.close` or `Cleanup`
/// restoring the stay-awake setting while the new session starts would corrupt it. A killed
/// helper restores nothing, so the new session takes the original from the journal persisted
/// on the phone (`/data/local/tmp/lantern-session.json`).
pub async fn wait_for_previous_helpers(adb: &Adb, bound: Duration) -> Result<()> {
    let deadline = Instant::now() + bound;
    let mut pids = helper_pids(adb).await?;
    if pids.is_empty() {
        return Ok(());
    }
    tracing::info!(?pids, "waiting for the previous helper to finish");
    while Instant::now() < deadline {
        tokio::time::sleep(PREVIOUS_HELPER_POLL.min(deadline - Instant::now())).await;
        pids = helper_pids(adb).await?;
        if pids.is_empty() {
            return Ok(());
        }
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    tracing::warn!(pids = list, ?bound, "previous helper still alive; killing it");
    if let Err(error) = adb.shell(&[&format!("kill -9 {list}")]).await {
        tracing::warn!(%error, "kill previous helper");
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let left = helper_pids(adb).await?;
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("previous helper processes {left:?} survived kill -9");
        }
        tokio::time::sleep(PREVIOUS_HELPER_POLL).await;
    }
}

/// Removes helper jars of other builds from the phone, keeping `remote`. Best effort.
async fn prune_old_jars(adb: &Adb, remote: &str) {
    let cmd = format!(
        "for f in /data/local/tmp/lantern-helper-*.jar; do [ \"$f\" = {remote} ] || rm -f \"$f\"; done"
    );
    if let Err(error) = adb.shell(&[&cmd]).await {
        tracing::warn!(%error, "could not prune old helper jars");
    }
}

/// Writes the embedded jar to a fresh, exclusively created host temp file (unique per call,
/// so concurrent starts never share or clobber one) and returns its path.
fn write_host_jar(sha: &str) -> Result<std::path::PathBuf> {
    use std::io::Write;
    let mut nonce = [0u8; 8];
    OsRng.fill_bytes(&mut nonce);
    let path = std::env::temp_dir().join(format!(
        "lantern-helper-{sha}-{}-{}.jar",
        std::process::id(),
        hex(&nonce)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    if let Err(error) = file.write_all(HELPER_JAR) {
        let _ = std::fs::remove_file(&path);
        return Err(error).context("write embedded helper jar");
    }
    Ok(path)
}

/// Removes the host temp jar when dropped (also when the start future is cancelled mid-push).
struct HostJar(std::path::PathBuf);

impl Drop for HostJar {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Starts a helper session. Every await is bounded, and the future is cancel-safe: dropping it
/// at any point kills the helper and removes any forward it created.
pub async fn start(adb: &Adb) -> Result<SessionConn> {
    // One daemon owns a serial, so every lantern forward of it that exists now is a leftover
    // of a daemon that died without cleaning up.
    adb.remove_stale_lantern_forwards().await;
    let helper_sha = hex(&Sha256::digest(HELPER_JAR));
    let remote = format!("/data/local/tmp/lantern-helper-{helper_sha}.jar");
    let installed = adb
        .shell(&["sha256sum", &remote])
        .await
        .map(|output| output.split_whitespace().next() == Some(helper_sha.as_str()))
        .unwrap_or(false);
    if !installed {
        let host = HostJar(write_host_jar(&helper_sha)?);
        adb.push(&host.0.to_string_lossy(), &remote).await?;
    }
    prune_old_jars(adb, &remote).await;
    // Never overlap a previous session's helper: see wait_for_previous_helpers.
    wait_for_previous_helpers(adb, PREVIOUS_HELPER_WAIT).await?;

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
        .and_then(|child| child.stdout.take())
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
    tracing::debug!(name, "helper listening");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("allocate local port")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    // Recorded before the call: if this future is cancelled mid-`adb forward`, the forward may
    // already exist server-side, and removing a missing forward is harmless.
    pending.forward = Some(port);
    adb.forward(port, &name).await?;
    tracing::debug!(port, name, "forward created");

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
    let stream = handshake(stream, &token).await?;

    let child = pending.child.take();
    let forward = pending.forward.take();
    Ok(SessionConn::from_parts(
        stream,
        adb.clone(),
        child,
        Some(stdout),
        forward,
        helper_sha,
    ))
}

/// A handshake that reached the helper but broke the protocol. Retrying cannot help: the helper
/// jar is embedded and content-addressed, so the same helper answers the same way every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    VersionMismatch { helper: u32, expected: u32 },
    TokenEcho,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch { helper, expected } => write!(
                f,
                "helper speaks protocol v{helper}, lantern expects v{expected}; rebuild/redeploy helper"
            ),
            Self::TokenEcho => f.write_str(
                "helper Hello echo carried a token; expected an empty token, refusing session",
            ),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl HandshakeError {
    /// Finds a protocol failure anywhere in `error`'s chain, under any added context.
    pub fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

/// Sends `Hello{PROTOCOL_VERSION, token}` and requires the helper's `Hello{PROTOCOL_VERSION, ""}`
/// echo.
pub async fn handshake(mut stream: TcpStream, token: &str) -> Result<TcpStream> {
    let hello = Control::Hello {
        version: PROTOCOL_VERSION,
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
        Control::Hello { version, token } if version == PROTOCOL_VERSION => {
            if token.is_empty() {
                Ok(stream)
            } else {
                Err(HandshakeError::TokenEcho.into())
            }
        }
        Control::Hello { version, .. } => Err(HandshakeError::VersionMismatch {
            helper: version,
            expected: PROTOCOL_VERSION,
        }
        .into()),
        Control::Error { msg, code, version } => match helper_version_error(&msg, code, version) {
            Some(helper) => Err(HandshakeError::VersionMismatch {
                helper,
                expected: PROTOCOL_VERSION,
            }
            .into()),
            None => bail!("helper rejected handshake: {msg}"),
        },
        other => bail!("expected hello reply, got {other:?}"),
    }
}

/// The helper's protocol version if its handshake `error` is a version mismatch. The helper
/// replies `error{code:"version", version:N}`; a helper from before the structured `code` only
/// has the message (`"protocol version mismatch: helper speaks N, ..."`). 0 means unknown.
fn helper_version_error(msg: &str, code: Option<String>, version: Option<u32>) -> Option<u32> {
    const PREFIX: &str = "protocol version mismatch";
    if code.as_deref() == Some("version") {
        return Some(version.unwrap_or(0));
    }
    if !msg.starts_with(PREFIX) {
        return None;
    }
    let parsed = msg
        .split_once("helper speaks ")
        .and_then(|(_, rest)| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
    Some(parsed.unwrap_or(0))
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
