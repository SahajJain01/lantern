use std::collections::BTreeSet;
use std::ffi::OsString;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_core::Stream;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// Bound on short adb calls (shell queries, forward add/remove).
pub const ADB_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on `adb push` of the helper jar.
pub const ADB_PUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on the synchronous forward removal done from `Drop` (cancelled start / dropped session).
pub const ADB_DROP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Adb {
    program: OsString,
    serial: String,
}

impl Adb {
    pub fn new(serial: impl Into<String>) -> Self {
        Self::with_program("adb", serial)
    }

    /// Uses `program` instead of `adb` from PATH (tests substitute a recording fake).
    pub fn with_program(program: impl Into<OsString>, serial: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            serial: serial.into(),
        }
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("-s")
            .arg(&self.serial)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Runs one adb invocation with a hard deadline; the child is killed on timeout or when the
    /// caller's future is dropped.
    async fn run(&self, mut command: Command, limit: Duration) -> Result<String> {
        let output = timeout(limit, command.output())
            .await
            .map_err(|_| anyhow!("adb -s {} timed out after {limit:?}", self.serial))?
            .context("run adb")?;
        if !output.status.success() {
            bail!(
                "adb -s {} failed ({}): {}",
                self.serial,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        String::from_utf8(output.stdout).context("adb output is not UTF-8")
    }

    pub async fn shell(&self, args: &[&str]) -> Result<String> {
        let mut command = self.command();
        command.arg("shell").args(args);
        self.run(command, ADB_CALL_TIMEOUT).await
    }

    pub async fn push(&self, local: &str, remote: &str) -> Result<()> {
        let mut command = self.command();
        command.arg("push").arg(local).arg(remote);
        self.run(command, ADB_PUSH_TIMEOUT).await.map(|_| ())
    }

    pub async fn forward(&self, local_tcp: u16, abstract_name: &str) -> Result<()> {
        let mut command = self.command();
        command
            .arg("forward")
            .arg(format!("tcp:{local_tcp}"))
            .arg(format!("localabstract:{abstract_name}"));
        self.run(command, ADB_CALL_TIMEOUT).await.map(|_| ())
    }

    pub async fn remove_forward(&self, local_tcp: u16) -> Result<()> {
        let mut command = self.command();
        command
            .arg("forward")
            .arg("--remove")
            .arg(format!("tcp:{local_tcp}"));
        self.run(command, ADB_CALL_TIMEOUT).await.map(|_| ())
    }

    /// Removes this serial's leftover `localabstract:lantern-*` forwards, which a daemon that was
    /// killed (SIGKILL, crash) could not remove itself. Best effort: failures are only logged.
    /// Call it before creating this session's forward.
    pub async fn remove_stale_lantern_forwards(&self) {
        let mut command = self.command();
        command.arg("forward").arg("--list");
        let list = match self.run(command, ADB_CALL_TIMEOUT).await {
            Ok(list) => list,
            Err(error) => {
                tracing::warn!(%error, "could not list adb forwards");
                return;
            }
        };
        for local in stale_lantern_forwards(&list, &self.serial) {
            let mut command = self.command();
            command.arg("forward").arg("--remove").arg(&local);
            match self.run(command, ADB_CALL_TIMEOUT).await {
                Ok(_) => tracing::info!(local, "removed stale lantern forward"),
                Err(error) => tracing::warn!(%error, local, "could not remove stale forward"),
            }
        }
    }

    /// Forward removal for `Drop` paths, which cannot await. Bounded by `ADB_DROP_TIMEOUT`; the
    /// adb client is killed if it has not finished by then.
    pub fn remove_forward_blocking(&self, local_tcp: u16) {
        let child = std::process::Command::new(&self.program)
            .arg("-s")
            .arg(&self.serial)
            .arg("forward")
            .arg("--remove")
            .arg(format!("tcp:{local_tcp}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%error, port = local_tcp, "could not spawn adb forward --remove");
                return;
            }
        };
        let deadline = std::time::Instant::now() + ADB_DROP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    tracing::warn!(port = local_tcp, "adb forward --remove timed out; killed");
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }

    pub fn spawn_shell(&self, cmd: &str) -> Result<Child> {
        let child = self
            .command()
            .arg("shell")
            .arg(cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn adb shell")?;
        Ok(child)
    }
}

/// The local specs (`tcp:<port>`) of `serial`'s forwards to a `localabstract:lantern-*` socket in
/// `adb forward --list` output. The list covers every device, one `serial local remote` per line.
pub fn stale_lantern_forwards(list: &str, serial: &str) -> Vec<String> {
    list.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next(), fields.next()) {
                (Some(owner), Some(local), Some(remote))
                    if owner == serial && remote.starts_with("localabstract:lantern-") =>
                {
                    Some(local.to_owned())
                }
                _ => None,
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Online(String),
    Offline(String),
}

/// One `serial\tstate` entry of a track-devices device list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    pub serial: String,
    pub state: String,
}

/// Reassembles `adb track-devices` output into complete device lists.
///
/// The wire format is a repeated `<4 hex digit length N><N bytes>` message, each payload being the
/// *full* current device list (`serial\tstate\n` per device). A removed device is not reported
/// as offline; it is simply absent, so an unplug of the only device is the 4 bytes `0000` with no
/// newline. Messages are not line-aligned and may be split across reads.
#[derive(Debug, Default)]
pub struct TrackDevicesDecoder {
    buf: Vec<u8>,
}

impl TrackDevicesDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<DeviceEntry>>> {
        self.buf.extend_from_slice(bytes);
        let mut lists = Vec::new();
        while self.buf.len() >= 4 {
            let prefix = &self.buf[..4];
            if !prefix.iter().all(u8::is_ascii_hexdigit) {
                bail!(
                    "adb track-devices: bad length prefix {:?}",
                    String::from_utf8_lossy(prefix)
                );
            }
            let len = usize::from_str_radix(std::str::from_utf8(prefix)?, 16)?;
            if self.buf.len() < 4 + len {
                break;
            }
            let payload: Vec<u8> = self.buf.drain(..4 + len).skip(4).collect();
            lists.push(parse_device_list(&String::from_utf8_lossy(&payload)));
        }
        Ok(lists)
    }
}

/// Splits one device-list payload. The state is everything after the first tab, so states with
/// spaces (`no permissions (...)`) survive intact.
pub fn parse_device_list(payload: &str) -> Vec<DeviceEntry> {
    payload
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match line.split_once('\t') {
            Some((serial, state)) => DeviceEntry {
                serial: serial.trim().to_owned(),
                state: state.trim().to_owned(),
            },
            None => {
                let mut fields = line.split_whitespace();
                DeviceEntry {
                    serial: fields.next().unwrap_or_default().to_owned(),
                    state: fields.collect::<Vec<_>>().join(" "),
                }
            }
        })
        .collect()
}

/// Diffs successive full device lists into edge events. Only state `device` counts as online;
/// leaving it (to any other state, or by disappearing) is `Offline`.
#[derive(Debug, Default)]
pub struct DeviceTracker {
    online: BTreeSet<String>,
}

impl DeviceTracker {
    pub fn update(&mut self, list: &[DeviceEntry]) -> Vec<DeviceEvent> {
        let now: BTreeSet<String> = list
            .iter()
            .filter(|entry| entry.state == "device")
            .map(|entry| entry.serial.clone())
            .collect();
        let mut events: Vec<DeviceEvent> = self
            .online
            .difference(&now)
            .cloned()
            .map(DeviceEvent::Offline)
            .collect();
        events.extend(
            now.difference(&self.online)
                .cloned()
                .map(DeviceEvent::Online),
        );
        self.online = now;
        events
    }
}

/// A `track-devices` client that stayed up this long resets the restart backoff.
const TRACK_STABLE_AFTER: Duration = Duration::from_secs(5);

/// Streams device edges from `adb track-devices`. If the adb client exits (e.g. the adb server
/// was killed) every known device is reported offline and the client is restarted with backoff;
/// the next full list then re-reports whatever is present.
pub fn track_devices() -> impl Stream<Item = DeviceEvent> {
    track_devices_with("adb")
}

pub fn track_devices_with(program: impl Into<OsString>) -> impl Stream<Item = DeviceEvent> {
    let program = program.into();
    async_stream::stream! {
        let mut tracker = DeviceTracker::default();
        let mut backoff = Duration::from_millis(250);
        loop {
            let spawned = tokio::time::Instant::now();
            let child = Command::new(&program)
                .arg("track-devices")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn();
            match child {
                Ok(mut child) => {
                    if let Some(mut stdout) = child.stdout.take() {
                        let mut decoder = TrackDevicesDecoder::default();
                        let mut buf = [0u8; 4096];
                        loop {
                            let n = match stdout.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => n,
                                Err(error) => {
                                    tracing::warn!(%error, "adb track-devices read failed");
                                    break;
                                }
                            };
                            let lists = match decoder.push(&buf[..n]) {
                                Ok(lists) => lists,
                                Err(error) => {
                                    tracing::warn!(%error, "adb track-devices stream corrupt");
                                    break;
                                }
                            };
                            for list in lists {
                                for event in tracker.update(&list) {
                                    yield event;
                                }
                            }
                        }
                    }
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    tracing::warn!("adb track-devices ended; restarting");
                }
                Err(error) => tracing::error!(%error, "failed to start adb track-devices"),
            }
            for event in tracker.update(&[]) {
                yield event;
            }
            // Only a client that stayed up resets the backoff; one that decodes a list and dies
            // at once (flapping adb) keeps backing off.
            if spawned.elapsed() >= TRACK_STABLE_AFTER {
                backoff = Duration::from_millis(250);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
#[path = "adb_tests.rs"]
mod tests;
