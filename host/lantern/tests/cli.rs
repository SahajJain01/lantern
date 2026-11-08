use std::process::Command;

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
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let exit = daemon.wait().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
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

#[test]
fn failed_open_prints_the_error_once_and_exits_1() {
    use std::io::{Read, Write};
    // The window process needs a Wayland connection for its event loop, but never opens a window
    // here (the open fails before `opened`).
    let wayland = match (
        std::env::var("WAYLAND_DISPLAY"),
        std::env::var("XDG_RUNTIME_DIR"),
    ) {
        (Ok(d), _) if d.starts_with('/') => d,
        (Ok(d), Ok(rt)) if !d.is_empty() => format!("{rt}/{d}"),
        _ => {
            eprintln!("skipped: no Wayland session");
            return;
        }
    };
    // Short: a Unix socket path must fit in sun_path (108 bytes).
    let dir = std::env::temp_dir().join(format!("lantern-f{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sock_dir = dir.join("lantern");
    std::fs::create_dir_all(&sock_dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(sock_dir.join("daemon.sock")).unwrap();
    let daemon = std::thread::spawn(move || {
        let (mut c, _) = listener.accept().unwrap();
        // The window's `open` frame.
        let mut head = [0u8; 9];
        c.read_exact(&mut head).unwrap();
        let len = u32::from_be_bytes(head[0..4].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len - 5];
        c.read_exact(&mut payload).unwrap();
        // What lanternd sends for an open the phone refused, then EOF.
        c.write_all(&control_frame(
            1,
            r#"{"t":"error","msg":"no such app on the phone"}"#,
        ))
        .unwrap();
        c.write_all(&control_frame(1, r#"{"t":"closed","reason":"error"}"#))
            .unwrap();
        drop(c);
        String::from_utf8(payload).unwrap()
    });
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(["open", "com.example"])
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("WAYLAND_DISPLAY", &wayland)
        .output()
        .unwrap();
    let open = daemon.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(open.contains("\"open\""), "first frame: {open}");
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    // The user-facing line (tracing log lines may mention the message too).
    let printed = stderr
        .lines()
        .filter(|l| l.starts_with("lantern: ") && l.contains("no such app on the phone"))
        .count();
    assert_eq!(printed, 1, "printed once: {stderr}");
}
