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
    let dir = std::env::temp_dir().join(format!("lantern-cli-{tag}-{}-{nanos}", std::process::id()));
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
    assert!(stderr.contains("lanternd is not running"), "stderr: {stderr}");
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
    std::fs::write(&adb, "#!/bin/sh\ncase \"$*\" in *track-devices*) exec sleep 30;; esac\nexit 1\n").unwrap();
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
        assert!(std::time::Instant::now() < deadline, "daemon never listened");
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
    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"state": "idle", "serial": null, "helper": null, "windows": []})
    );
    assert_eq!(exit.code(), Some(0), "SIGTERM is a clean daemon exit");
}
