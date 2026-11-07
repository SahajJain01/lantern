//! Runs the real `session::start` against a recording fake `adb` executable, to prove that a
//! cancelled start (SIGINT / Offline drops the future) kills the helper and removes the forward,
//! and that every adb call is bounded.

use std::path::PathBuf;
use std::time::Duration;

use lantern::adb::Adb;
use lantern::daemon::session::start;
use tokio::time::Instant;

struct FakeAdb {
    dir: PathBuf,
    program: PathBuf,
    log: PathBuf,
    pidfile: PathBuf,
}

impl FakeAdb {
    /// `forward_delay` makes `adb forward` sleep that long before succeeding.
    fn new(tag: &str, forward_delay: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lantern-fake-adb-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("adb");
        let log = dir.join("calls.log");
        let pidfile = dir.join("helper.pid");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
shift 2
case "$1" in
  shell)
    if [ "$2" = sha256sum ]; then exit 1; fi
    name=$(printf '%s\n' "$2" | awk '{{print $(NF-1)}}')
    echo $$ > '{pidfile}'
    echo "listening $name"
    exec sleep 1000
    ;;
  push) exit 0 ;;
  forward)
    if [ "$2" != --remove ]; then sleep {forward_delay}; fi
    exit 0
    ;;
esac
exit 0
"#,
            log = log.display(),
            pidfile = pidfile.display(),
        );
        std::fs::write(&program, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            program,
            log,
            pidfile,
        }
    }

    fn adb(&self) -> Adb {
        Adb::with_program(&self.program, "fake-serial")
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn helper_pid(&self) -> u32 {
        std::fs::read_to_string(&self.pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

impl Drop for FakeAdb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Alive = exists and is not a zombie awaiting reaping.
fn process_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            !state.starts_with('Z')
        }
        Err(_) => false,
    }
}

async fn wait_dead(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(pid) {
        assert!(Instant::now() < deadline, "helper pid {pid} still alive");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn forward_port(calls: &[String]) -> String {
    let line = calls
        .iter()
        .find(|c| c.starts_with("-s fake-serial forward tcp:"))
        .expect("forward was never created");
    line.split_whitespace().nth(3).unwrap().to_owned()
}

fn has_forward(calls: &[String]) -> bool {
    calls
        .iter()
        .any(|c| c.starts_with("-s fake-serial forward tcp:"))
}

/// Drives `start` until the fake has logged the `adb forward` call (plus `grace`), then drops
/// the future, as SIGINT or an Offline would. Panics if start finishes first.
async fn cancel_start_after_forward(fake: &FakeAdb, grace: Duration) {
    let adb = fake.adb();
    let fut = start(&adb);
    tokio::pin!(fut);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        tokio::select! {
            result = &mut fut => panic!("start finished before cancel: ok={}", result.is_ok()),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                if has_forward(&fake.calls()) {
                    break;
                }
                assert!(Instant::now() < deadline, "start never reached adb forward");
            }
        }
    }
    tokio::select! {
        result = &mut fut => panic!("start finished before cancel: ok={}", result.is_ok()),
        _ = tokio::time::sleep(grace) => {}
    }
}

#[tokio::test]
async fn cancelled_start_removes_forward_and_kills_helper() {
    let fake = FakeAdb::new("connect", "0");
    // Nothing listens behind the fake forward, so start() sits in its 3 s connect-retry loop;
    // cancel it there (shortly after the forward call), as SIGINT would.
    cancel_start_after_forward(&fake, Duration::from_millis(200)).await;
    let calls = fake.calls();
    let port = forward_port(&calls);
    assert!(
        calls.contains(&format!("-s fake-serial forward --remove {port}")),
        "forward not removed after cancel: {calls:#?}"
    );
    wait_dead(fake.helper_pid()).await;
}

#[tokio::test]
async fn cancel_while_adb_forward_runs_still_removes_it() {
    let fake = FakeAdb::new("forwarding", "30");
    // The fake logs the call before sleeping 30 s, so this cancels mid-`adb forward`.
    cancel_start_after_forward(&fake, Duration::ZERO).await;
    let calls = fake.calls();
    let port = forward_port(&calls);
    assert!(
        calls.contains(&format!("-s fake-serial forward --remove {port}")),
        "in-flight forward not removed after cancel: {calls:#?}"
    );
    wait_dead(fake.helper_pid()).await;
}

#[tokio::test]
async fn failed_start_cleans_up_too() {
    let fake = FakeAdb::new("fail", "0");
    let t0 = Instant::now();
    let error = start(&fake.adb())
        .await
        .err()
        .expect("no helper behind the forward");
    assert!(
        format!("{error:#}").contains("connect to helper"),
        "unexpected error: {error:#}"
    );
    assert!(t0.elapsed() < Duration::from_secs(5));
    let calls = fake.calls();
    let port = forward_port(&calls);
    assert!(calls.contains(&format!("-s fake-serial forward --remove {port}")));
    wait_dead(fake.helper_pid()).await;
}

/// A flapping adb: every `track-devices` client prints one list and exits at once. Decoding a
/// list must not reset the restart backoff (only a client that stayed up >= 5 s does), so the
/// restarts slow down 250 ms -> 500 ms -> 1 s instead of hammering every 250 ms.
#[tokio::test]
async fn flapping_track_devices_backs_off() {
    use futures_util::StreamExt;
    use lantern::adb::{track_devices_with, DeviceEvent};

    let fake = FakeAdb::new("flap", "0");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '000bdev\\tdevice\\n'\nexit 0\n",
        fake.log.display()
    );
    std::fs::write(&fake.program, script).unwrap();
    let events = track_devices_with(&fake.program);
    tokio::pin!(events);
    let mut onlines = 0;
    let window = tokio::time::sleep(Duration::from_millis(1600));
    tokio::pin!(window);
    loop {
        tokio::select! {
            event = events.next() => {
                if matches!(event, Some(DeviceEvent::Online(ref s)) if s == "dev") {
                    onlines += 1;
                }
            }
            _ = &mut window => break,
        }
    }
    // Spawns at ~0, 0.25, 0.75 s (next at 1.75 s). Without backoff growth: ~6-7.
    assert!(
        (2..=4).contains(&onlines),
        "track-devices restarted {onlines} times in 1.6 s"
    );
    let spawns = fake.calls().len();
    assert!(
        (onlines..=onlines + 1).contains(&spawns),
        "spawns={spawns} onlines={onlines}"
    );
}
