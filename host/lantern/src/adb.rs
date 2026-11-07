use std::collections::HashMap;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use futures_core::Stream;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command as AsyncCommand};

#[derive(Debug, Clone)]
pub struct Adb {
    serial: String,
}

impl Adb {
    pub fn new(serial: impl Into<String>) -> Self {
        Self {
            serial: serial.into(),
        }
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }

    fn command(&self) -> Command {
        let mut command = Command::new("adb");
        command.arg("-s").arg(&self.serial);
        command
    }

    fn run(&self, command: &mut Command) -> Result<String> {
        let output = command.output().context("run adb")?;
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

    pub fn shell(&self, args: &[&str]) -> Result<String> {
        self.run(self.command().arg("shell").args(args))
    }

    pub fn push(&self, local: &str, remote: &str) -> Result<()> {
        self.run(self.command().arg("push").arg(local).arg(remote))
            .map(|_| ())
    }

    pub fn forward(&self, local_tcp: u16, abstract_name: &str) -> Result<()> {
        self.run(
            self.command()
                .arg("forward")
                .arg(format!("tcp:{local_tcp}"))
                .arg(format!("localabstract:{abstract_name}")),
        )
        .map(|_| ())
    }

    pub fn remove_forward(&self, local_tcp: u16) -> Result<()> {
        self.run(
            self.command()
                .arg("forward")
                .arg("--remove")
                .arg(format!("tcp:{local_tcp}")),
        )
        .map(|_| ())
    }

    pub fn spawn_shell(&self, cmd: &str) -> Result<Child> {
        let child = AsyncCommand::new("adb")
            .arg("-s")
            .arg(&self.serial)
            .arg("shell")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn adb shell")?;
        Ok(child)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    Online(String),
    Offline(String),
}

pub fn parse_track_devices_lines(input: &str) -> Vec<DeviceEvent> {
    input
        .lines()
        .filter_map(|line| {
            let entry = match line.get(..4) {
                Some(prefix) if prefix.bytes().all(|b| b.is_ascii_hexdigit()) => &line[4..],
                _ => line,
            };
            let mut fields = entry.split_whitespace();
            let serial = fields.next()?.to_owned();
            match fields.next()? {
                "device" => Some(DeviceEvent::Online(serial)),
                "offline" | "unauthorized" => Some(DeviceEvent::Offline(serial)),
                _ => None,
            }
        })
        .collect()
}

pub fn track_devices() -> impl Stream<Item = DeviceEvent> {
    async_stream::stream! {
        let child = AsyncCommand::new("adb")
            .arg("track-devices")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                tracing::error!(%error, "failed to start adb track-devices");
                return;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            tracing::error!("adb track-devices has no stdout");
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        let mut states = HashMap::<String, bool>::new();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    for event in parse_track_devices_lines(&line) {
                        let (serial, online) = match &event {
                            DeviceEvent::Online(serial) => (serial.clone(), true),
                            DeviceEvent::Offline(serial) => (serial.clone(), false),
                        };
                        if states.insert(serial, online) == Some(online) {
                            continue;
                        }
                        yield event;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(%error, "adb track-devices read failed");
                    break;
                }
            }
        }
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_track_devices_lines() {
        let events =
            super::parse_track_devices_lines("001820e9318\tdevice\n001820e9318\toffline\n");
        assert_eq!(
            events,
            [
                DeviceEvent::Online("20e9318".into()),
                DeviceEvent::Offline("20e9318".into()),
            ]
        );
    }
}
