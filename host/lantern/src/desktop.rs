//! The graphical session's environment for desktop commands lanternd runs (`kdialog`,
//! `xdg-open`, `notify-send`) and for its own Wayland clipboard connection.
//!
//! lanternd is a systemd user service and may start before the session exported its display
//! variables, so a variable missing from our own environment is taken from the user manager
//! (`systemctl --user show-environment`).

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The variables a desktop command needs to reach the session.
pub const SESSION_VARS: [&str; 3] = ["WAYLAND_DISPLAY", "DISPLAY", "XDG_CURRENT_DESKTOP"];

/// How long `systemctl --user show-environment` may take.
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// [`SESSION_VARS`] that are set: ours, else the systemd user manager's.
pub fn session_env() -> Vec<(String, String)> {
    session_vars(&SESSION_VARS)
}

/// `names` that are set (non-empty): ours, else the systemd user manager's. systemctl is only
/// asked when one of them is missing here.
pub(crate) fn session_vars(names: &[&str]) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for &name in names {
        match std::env::var(name).ok().filter(|v| !v.is_empty()) {
            Some(value) => found.push((name.to_owned(), value)),
            None => missing.push(name),
        }
    }
    if !missing.is_empty() {
        let manager = systemd_environment().unwrap_or_default();
        for name in missing {
            if let Some(value) = lookup(&manager, name) {
                found.push((name.to_owned(), value.to_owned()));
            }
        }
    }
    found
}

/// `name`'s non-empty value in `show-environment` output (`KEY=value` lines).
fn lookup<'a>(environment: &'a str, name: &str) -> Option<&'a str> {
    environment
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        .filter(|v| !v.is_empty())
}

/// `systemctl --user show-environment`, or `None` if it fails or takes longer than
/// [`SYSTEMCTL_TIMEOUT`] (it is then killed).
fn systemd_environment() -> Option<String> {
    let mut child = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on a thread so a full pipe never stalls the child while we wait for it.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).ok().map(|_| out)
    });
    let deadline = Instant::now() + SYSTEMCTL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                tracing::warn!("systemctl --user show-environment timed out");
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok().flatten();
    status.filter(|s| s.success()).and(out)
}

/// Runs a desktop command with [`session_env`], stdin null; it outlives a dropped handle.
pub fn spawn<S: AsRef<OsStr>>(
    program: impl AsRef<OsStr>,
    args: &[S],
) -> std::io::Result<tokio::process::Child> {
    tokio::process::Command::new(program)
        .args(args)
        .envs(session_env())
        .stdin(Stdio::null())
        .kill_on_drop(false)
        .spawn()
}

/// A blocking desktop command with [`session_env`] and stdin null, for a caller that reads
/// its output (the PIN dialog). Building it may block on systemctl.
pub fn command<S: AsRef<OsStr>>(program: impl AsRef<OsStr>, args: &[S]) -> Command {
    let mut command = Command::new(program);
    command.args(args).envs(session_env()).stdin(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_exact_names_only() {
        let env =
            "DISPLAY=:0\nWAYLAND_DISPLAY_X=no\nWAYLAND_DISPLAY=wayland-0\nXDG_CURRENT_DESKTOP=\n";
        assert_eq!(lookup(env, "WAYLAND_DISPLAY"), Some("wayland-0"));
        assert_eq!(lookup(env, "DISPLAY"), Some(":0"));
        assert_eq!(lookup(env, "XDG_CURRENT_DESKTOP"), None);
        assert_eq!(lookup(env, "HOME"), None);
    }

    #[tokio::test]
    async fn spawn_runs_the_command() {
        let mut child = spawn("true", &[] as &[&str]).unwrap();
        assert!(child.wait().await.unwrap().success());
    }
}
