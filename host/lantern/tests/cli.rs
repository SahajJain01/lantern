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
    let (output, sent) = open_without_daemon("open-none", &["open", "com.example"]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("lanternd is not running"),
        "stderr: {stderr}"
    );
    assert!(sent.contains("Connect your phone"), "notify-send: {sent:?}");
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

/// Scratch `XDG_DATA_HOME`/`XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_CACHE_HOME` under `dir`: a
/// `lantern` run from a test must never read or write the desktop's menu entries, menu files,
/// settings, state or caches (lanternd migrates the menu at startup).
fn scratch_xdg(dir: &std::path::Path) -> [(&'static str, std::path::PathBuf); 4] {
    [
        ("XDG_DATA_HOME", dir.join("data")),
        ("XDG_CONFIG_HOME", dir.join("config")),
        ("XDG_STATE_HOME", dir.join("state")),
        ("XDG_CACHE_HOME", dir.join("cache")),
    ]
}

/// Stubs in `bin` (put first on `PATH`) for the desktop tools lanternd may run: a
/// `kbuildsycoca6` that only appends a line to the returned file (the real one rebuilds the
/// desktop's menu cache), and a `notify-send` that does nothing.
fn stub_desktop_tools(bin: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let ran = bin.join("kbuildsycoca6.ran");
    for (name, body) in [
        (
            "kbuildsycoca6",
            format!("#!/bin/sh\necho ran >> '{}'\nexit 0\n", ran.display()),
        ),
        ("notify-send", "#!/bin/sh\nexit 0\n".to_owned()),
    ] {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    ran
}

/// A spawned `lantern daemon`, SIGTERMed and reaped when dropped (also on a failed assert).
struct StopOnDrop(std::process::Child);

impl StopOnDrop {
    fn stop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let _ = self.0.wait();
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop();
    }
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
        .envs(scratch_xdg(&dir))
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
            .envs(scratch_xdg(&dir))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{cmd} stderr: {stderr}");
        assert!(
            stderr.contains("lanternd is not running"),
            "stderr: {stderr}"
        );
    }
}

/// `lantern connect|disconnect [<serial>]`: one optional serial; a third argument is a usage
/// error (exit 2) that never reaches a daemon. Every run gets a scratch runtime dir, so none
/// can reach the desktop's lanternd.
#[test]
fn connect_takes_one_optional_serial() {
    for cmd in ["connect", "disconnect"] {
        let dir = scratch("cmd-serial");
        let run = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_lantern"))
                .args(args)
                .env("XDG_RUNTIME_DIR", &dir)
                .envs(scratch_xdg(&dir))
                .output()
                .unwrap()
        };
        // A serial is accepted: the command then needs a daemon.
        let output = run(&[cmd, "20e9318"]);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{cmd} <serial>: {stderr}");
        assert!(stderr.contains("lanternd is not running"), "{stderr}");
        for args in [&[cmd, "a", "b"][..], &[cmd, "--serial"][..], &[cmd, ""][..]] {
            let usage = run(args);
            assert_eq!(usage.status.code(), Some(2), "{args:?}");
            let text = String::from_utf8(usage.stderr).unwrap();
            assert!(
                text.contains(&format!("lantern {cmd} [<serial>]")),
                "usage: {text}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn daemon_runtime_failure_exits_1() {
    // No XDG_RUNTIME_DIR: the daemon cannot place its socket. That is a runtime failure, not
    // a usage error (2) and not a protocol mismatch (3).
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env_remove("XDG_RUNTIME_DIR")
        .envs(scratch_xdg(&scratch("no-runtime")))
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
    stub_desktop_tools(&bin);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
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
        .envs(scratch_xdg(&dir))
        .output()
        .unwrap();
    // `lantern disconnect` with no phone plugged in: lanternd's error, exit 1.
    let disconnect = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("disconnect")
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
        .output()
        .unwrap();
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let exit = daemon.wait().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let refused = String::from_utf8_lossy(&disconnect.stderr);
    assert_eq!(disconnect.status.code(), Some(1), "{refused}");
    assert!(
        refused.contains("lanternd: no phone is plugged in"),
        "{refused}"
    );
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
            .envs(scratch_xdg(&dir))
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
        let (output, _) = open_without_daemon("open-user-ok", args);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?} stderr: {stderr}");
        assert!(
            stderr.contains("lanternd is not running"),
            "{args:?} stderr: {stderr}"
        );
    }
}

/// Runs `lantern open <args>` with no daemon, in scratch XDG dirs, a dead session bus and a
/// recording `notify-send` stub (a test never shows a notification on the desktop); returns
/// (output, the notify-send argument lines).
fn open_without_daemon(tag: &str, args: &[&str]) -> (std::process::Output, String) {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch(tag);
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    stub_desktop_tools(&bin);
    let notes = dir.join("notify-send.args");
    let stub = bin.join("notify-send");
    std::fs::write(
        &stub,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", notes.display()),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .args(args)
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
        .env("PATH", &path)
        .output()
        .unwrap();
    // notify-send is reaped off-thread; give a late write a moment.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !notes.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let sent = std::fs::read_to_string(&notes).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    (output, sent)
}

#[test]
fn open_with_phone_reaches_the_daemon_step() {
    // A valid `--phone` parses; with no daemon it fails at runtime (1), not as usage (2), and
    // shows the \"Connect your phone\" notice (through the stub).
    for args in [
        &["open", "--phone", "20e9318", "com.example"][..],
        &["open", "--phone=ZD222LMDFV", "--user", "10", "com.example"],
        &["open", "com.example", "--phone", "a:b"],
    ] {
        let (output, sent) = open_without_daemon("open-phone-ok", args);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?} stderr: {stderr}");
        assert!(
            stderr.contains("lanternd is not running"),
            "{args:?} stderr: {stderr}"
        );
        assert!(
            sent.contains("Connect your phone"),
            "{args:?} notify-send: {sent:?}"
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
            .envs(scratch_xdg(&dir))
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
        // Named: without a phone the window first asks for `lantern status`, which this fake
        // daemon doesn't answer.
        .args(["open", "--phone", "dev", "com.example"])
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
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
    // Never reach the desktop's notification daemon or menu cache from a test.
    stub_desktop_tools(&bin);
    // The phone connects as soon as it is plugged in (the default asks first).
    let settings = dir.join("config").join("lantern").join("settings.toml");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "autoconnect = true\n").unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
        // No tray on the real session bus from a test: the daemon logs "no tray" and runs on.
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
        .env("PATH", &path)
        .env_remove("LANTERN_SERIAL")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Stopped even if an assertion below fails (no daemon outlives the test).
    let mut daemon = StopOnDrop(daemon);
    let deadline = Instant::now() + Duration::from_secs(5);
    let starting = loop {
        let output = Command::new(env!("CARGO_BIN_EXE_lantern"))
            .arg("status")
            .env("XDG_RUNTIME_DIR", &dir)
            .envs(scratch_xdg(&dir))
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
        .envs(scratch_xdg(&dir))
        .env("PATH", &path)
        .output()
        .unwrap();
    let calls_after = std::fs::read_to_string(&log).unwrap_or_default();
    daemon.stop();
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
    stub_desktop_tools(&bin);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
        // No tray on the real session bus from a test: the daemon logs "no tray" and runs on.
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
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
            .envs(scratch_xdg(&dir))
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
        .envs(scratch_xdg(&dir))
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

/// lanternd migrates the menu (and records the primary phone in `menu.json`) before it answers
/// any client, so no window or session is served with names from a missing `menu.json`. Only
/// the scratch XDG dirs are used.
#[test]
fn daemon_migrates_the_menu_before_it_answers() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("migrate");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let adb = bin.join("adb");
    std::fs::write(
        &adb,
        "#!/bin/sh\ncase \"$*\" in *track-devices*) exec sleep 30;; esac\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    let apps = dir.join("data/applications");
    std::fs::create_dir_all(&apps).unwrap();
    for (pkg, serials) in [("com.a", "A;"), ("com.a2", "A;"), ("com.s", "A;B;")] {
        std::fs::write(
            apps.join(format!("lantern.{pkg}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={pkg}\nExec=lantern open {pkg}\n\
                 Categories=X-Lantern;\nStartupWMClass=lantern.{pkg}\nX-Lantern-Serial=A\n\
                 X-Lantern-Serials={serials}\nX-Lantern-Package={pkg}\nX-Lantern-User=0\n"
            ),
        )
        .unwrap();
    }
    let rebuilt = stub_desktop_tools(&bin);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_lantern"))
        .arg("daemon")
        .env("XDG_RUNTIME_DIR", &dir)
        .envs(scratch_xdg(&dir))
        .env("DBUS_SESSION_BUS_ADDRESS", no_bus(&dir))
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
            .envs(scratch_xdg(&dir))
            .output()
            .unwrap();
        if output.status.success() {
            break;
        }
        assert!(Instant::now() < deadline, "lanternd never answered");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The first answer comes after the migration.
    let menu = std::fs::read_to_string(dir.join("state/lantern/menu.json"));
    let shared_b = std::fs::read_to_string(apps.join("lantern.com.s-pB.desktop"));
    // The menu changed, so lanternd rebuilt the menu cache: the stub ran, not the real
    // kbuildsycoca6 (spawned without waiting, so give it a moment).
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rebuilt.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let rebuilt = rebuilt.exists();
    unsafe { libc::kill(daemon.id() as i32, libc::SIGTERM) };
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&dir);
    let menu: serde_json::Value = serde_json::from_str(&menu.expect("menu.json")).unwrap();
    assert_eq!(menu, serde_json::json!({"version": 2, "primary": "A"}));
    assert!(rebuilt, "the kbuildsycoca6 stub never ran");
    let shared_b = shared_b.expect("B's own entry");
    assert!(
        shared_b.contains("StartupWMClass=lantern.com.s-pB\n"),
        "{shared_b}"
    );
    assert!(shared_b.contains(" open --phone B com.s\n"), "{shared_b}");
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
