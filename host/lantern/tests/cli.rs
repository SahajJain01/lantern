use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn unknown_subcommand_is_a_clear_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("unknown")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown subcommand"), "stderr: {stderr}");
    assert!(stderr.contains("daemon"), "stderr: {stderr}");
}

#[test]
fn usage_error_exits_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["open"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn open_without_a_daemon_exits_1() {
    let dir = std::env::temp_dir().join(format!("lantern-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["open", "com.example"])
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("lanternd is not running"),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_subcommand_prints_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage: lantern daemon"), "stderr: {stderr}");
}

/// A session bus address nobody listens on, inside the test's scratch `dir`.
fn no_bus(dir: &std::path::Path) -> String {
    format!("unix:path={}", dir.join("no-bus").display())
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("lantern-cli-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn status_without_a_daemon_exits_1() {
    let dir = scratch("status-none");
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("status")
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("lanternd is not running"),
        "stderr: {stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn status_with_arguments_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["status", "extra"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn connect_and_disconnect_without_a_daemon_exit_1() {
    for cmd in ["connect", "disconnect"] {
        let dir = scratch("cmd-none");
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .arg(cmd)
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{cmd} stderr: {stderr}");
        assert!(
            stderr.contains("lanternd is not running"),
            "stderr: {stderr}"
        );
        let usage = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args([cmd, "extra"])
            .output()
            .unwrap();
        assert_eq!(usage.status.code(), Some(2), "{cmd} extra");
        let text = String::from_utf8(usage.stderr).unwrap();
        assert!(text.contains(&format!("lantern {cmd}")), "usage: {text}");
    }
}

#[test]
fn daemon_runtime_failure_exits_1() {
    // No XDG_RUNTIME_DIR: the daemon cannot place its socket. That is a runtime failure, not
    // a usage error (2) and not a protocol mismatch (3).
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("XDG_RUNTIME_DIR"), "stderr: {stderr}");
}

#[test]
fn status_prints_the_running_daemons_state_as_json() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("status-live");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // An adb whose track-devices never lists a device: the daemon stays idle.
    let adb = bin.join("adb");
    std::fs::write(
        &adb,
        "#!/bin/sh\ncase \"$*\" in *track-devices*) exec sleep 30;; esac\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        // No tray on the real session bus from a test: the daemon logs "no tray" and runs on.
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
        .env("PATH", &path)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let sock = dir.join("lantern/daemon.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !sock.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "daemon never listened"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("status")
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    // `lantern disconnect` with nothing live: the report, exit 0, still idle.
    let disconnect = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("disconnect")
        .env("XDG_RUNTIME_DIR", &dir)
        .output()
        .unwrap();
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let exit = daemon.wait().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(disconnect.status.code(), Some(0));
    let reply: serde_json::Value = serde_json::from_slice(&disconnect.stdout).unwrap();
    assert_eq!(reply["state"], "idle");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"state": "idle", "serial": null, "helper": null, "windows": []})
    );
    assert_eq!(exit.code(), Some(0), "SIGTERM is a clean daemon exit");
}

#[test]
fn open_user_usage_errors_exit_2() {
    for args in [
        &["open", "--user"][..],
        &["open", "--user", "x", "com.example"],
        &["open", "--user", "-1", "com.example"],
        &["open", "--user", "2147483648", "com.example"],
        &["open", "--user", "10"],
        &["open", "--bogus", "com.example"],
    ] {
        let dir = scratch("open-user-usage");
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?} stderr: {stderr}");
        assert!(stderr.contains("usage:"), "{args:?} stderr: {stderr}");
    }
}

#[test]
fn open_with_user_reaches_the_daemon_step() {
    // A valid `--user` parses; with no daemon it fails at runtime (1), not as usage (2).
    for args in [
        &["open", "--user", "10", "com.example"][..],
        &["open", "--user=10", "com.example"],
        &["open", "com.example", "--user", "0"],
    ] {
        let dir = scratch("open-user-ok");
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?} stderr: {stderr}");
        assert!(
            stderr.contains("lanternd is not running"),
            "{args:?} stderr: {stderr}"
        );
    }
}

#[test]
fn open_rejects_bad_packages_and_user_forms() {
    for args in [
        &["open", "com.exa mple"][..],
        &["open", "com.example;rm"],
        &["open", "com/example"],
        &["open", "com.example-u10"],
        &["open", "--user", "+10", "com.example"],
        &["open", "--user", "010", "com.example"],
        &["open", "--user=00", "com.example"],
    ] {
        let dir = scratch("open-bad-pkg");
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(args)
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?} stderr: {stderr}");
        assert!(stderr.contains("usage:"), "{args:?} stderr: {stderr}");
    }
}

/// One frame of the window<->daemon wire: len (payload + 5), kind, stream, payload.
fn control_frame(stream: u32, json: &str) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&(json.len() as u32 + 5).to_be_bytes());
    f.push(1);
    f.extend_from_slice(&stream.to_be_bytes());
    f.extend_from_slice(json.as_bytes());
    f
}

/// Waits for `child` up to `limit`, killing it on timeout; returns its output.
fn output_within(mut child: std::process::Child, limit: Duration) -> std::process::Output {
    let deadline = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "lantern open did not exit within {limit:?}; stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[test]
#[ignore = "needs a Wayland session: the window process opens its event loop before the daemon answers (run with --ignored in a desktop session)"]
fn failed_open_prints_the_error_once_and_exits_1() {
    use std::io::{Read, Write};
    // The window process needs a Wayland connection for its event loop, but never opens a window
    // here (the open fails before `opened`). Ignored by default; when run, a missing session is a
    // failure, not a silent pass.
    let wayland = match (
        std::env::var("WAYLAND_DISPLAY"),
        std::env::var("XDG_RUNTIME_DIR"),
    ) {
        (Ok(d), _) if d.starts_with('/') => d,
        (Ok(d), Ok(rt)) if !d.is_empty() => format!("{rt}/{d}"),
        _ => panic!("no Wayland session (WAYLAND_DISPLAY unset): this test needs one"),
    };
    // Short: a Unix socket path must fit in sun_path (108 bytes).
    let dir = std::env::temp_dir().join(format!("lantern-f{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sock_dir = dir.join("lantern");
    std::fs::create_dir_all(&sock_dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(sock_dir.join("daemon.sock")).unwrap();
    // Never block forever: the window may fail before connecting or before sending `open`.
    listener.set_nonblocking(true).unwrap();
    let daemon = std::thread::spawn(move || -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut c = loop {
            match listener.accept() {
                Ok((c, _)) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return Err("the window never connected".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(format!("accept: {e}")),
            }
        };
        c.set_nonblocking(false).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        // The window's `open` frame.
        let mut head = [0u8; 9];
        c.read_exact(&mut head)
            .map_err(|e| format!("no open frame: {e}"))?;
        let len = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len - 5];
        c.read_exact(&mut payload)
            .map_err(|e| format!("short open frame: {e}"))?;
        // What lanternd sends for an open the phone refused, then EOF.
        c.write_all(&control_frame(
            1,
            r#"{"t":"error","msg":"no such app on the phone"}"#,
        ))
        .unwrap();
        c.write_all(&control_frame(1, r#"{"t":"closed","reason":"error"}"#))
            .unwrap();
        drop(c);
        Ok(String::from_utf8(payload).unwrap())
    });
    let child = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["open", "com.example"])
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("WAYLAND_DISPLAY", &wayland)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let output = output_within(child, Duration::from_secs(20));
    let open = daemon.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let open = open.unwrap_or_else(|e| panic!("{e}; stderr: {stderr}"));
    assert!(open.contains("\"open\""), "first frame: {open}");
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    // The user-facing line (tracing log lines may mention the message too).
    let printed = stderr
        .lines()
        .filter(|l| l.starts_with("lantern: ") && l.contains("no such app on the phone"))
        .count();
    assert_eq!(printed, 1, "printed once: {stderr}");
}

/// While lanternd is starting or serving a session on the phone, undo-setup must not touch it
/// (the helper is restoring/using the settings and the companion).
#[test]
fn undo_setup_refuses_live_session() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("undo-live");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("adb.log");
    // One device online; every other adb call hangs, so the session stays `starting`.
    let adb = bin.join("adb");
    std::fs::write(
        &adb,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$*\" in *track-devices*) printf '000bdev\\tdevice\\n'; exec sleep 30;; esac\nexec sleep 30\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Never reach the desktop's notification daemon from a test.
    let notify = bin.join("notify-send");
    std::fs::write(&notify, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&notify, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        // No tray on the real session bus from a test: the daemon logs "no tray" and runs on.
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("PATH", &path)
        .env_remove("LANTERN_SERIAL")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let starting = loop {
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .arg("status")
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains("\"starting\"") {
            break stdout;
        }
        assert!(Instant::now() < deadline, "never starting: {stdout}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(starting.contains("\"serial\":\"dev\""), "{starting}");
    let calls_before = std::fs::read_to_string(&log).unwrap_or_default();
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["undo-setup", "--serial", "dev"])
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("PATH", &path)
        .output()
        .unwrap();
    let calls_after = std::fs::read_to_string(&log).unwrap_or_default();
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(lantern::undo::STOP_FIRST),
        "stderr: {stderr}"
    );
    // undo-setup itself made no adb call (the daemon's own calls may continue meanwhile).
    let new_calls = &calls_after[calls_before.len().min(calls_after.len())..];
    assert!(
        !new_calls.contains("--forget") && !new_calls.contains("uninstall"),
        "undo-setup touched the phone: {new_calls}"
    );
}

/// An idle lanternd may be waiting to retry a session, so undo-setup refuses whenever lanternd
/// answers at all, not only while a session is live.
#[test]
fn undo_setup_refuses_idle_daemon() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("undo-idle");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.join("adb.log");
    // No device online (the daemon stays idle); every other adb call fails fast.
    let adb = bin.join("adb");
    std::fs::write(
        &adb,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$*\" in *track-devices*) exec sleep 30;; esac\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let notify = bin.join("notify-send");
    std::fs::write(&notify, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&notify, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        // No tray on the real session bus from a test: the daemon logs "no tray" and runs on.
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("PATH", &path)
        .env_remove("LANTERN_SERIAL")
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .arg("status")
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if stdout.contains("\"idle\"") {
            break;
        }
        assert!(Instant::now() < deadline, "never idle: {stdout}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let calls_before = std::fs::read_to_string(&log).unwrap_or_default();
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["undo-setup", "--serial", "dev"])
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("PATH", &path)
        .output()
        .unwrap();
    let calls_after = std::fs::read_to_string(&log).unwrap_or_default();
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(lantern::undo::STOP_FIRST),
        "stderr: {stderr}"
    );
    let new_calls = &calls_after[calls_before.len().min(calls_after.len())..];
    assert!(
        !new_calls.contains("-s dev"),
        "undo-setup touched the phone: {new_calls}"
    );
}

#[test]
fn undo_setup_usage_errors_exit_2() {
    for args in [
        &["undo-setup", "--serial"][..],
        &["undo-setup", "extra"],
        &["undo-setup", "--serial", ""],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
}
