//! Runs the real `session::start` against a recording fake `adb` executable, to prove that a
//! cancelled start (SIGINT / Offline drops the future) kills the helper and removes the forward,
//! and that every adb call is bounded.
//!
//! The fake scripts are written once per process (under a `OnceLock`, before any test spawns
//! anything) into a shared directory; each test gets its own directory holding a *symlink* to
//! the script plus its data files, which the script finds via `dirname "$0"`. So no test ever
//! has an executable open for writing while another thread forks: a fork that inherits such an
//! fd makes the exec fail with ETXTBSY.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use lantern::adb::Adb;
use lantern::daemon::session::{start, wait_for_previous_helpers};
use tokio::time::Instant;

/// The main fake. Per-test files (all in the symlink's directory): `calls.log` (every
/// invocation's args), `helper.pid` (pid of the fake helper), `forwards` (`forward --list`
/// output), `forward_delay` (seconds `forward` sleeps), `helpers` (pids `pgrep` reports) and
/// `polls` (after that many `pgrep`s the helpers are gone; absent = never).
const ADB_SCRIPT: &str = r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\n' "$*" >> "$d/calls.log"
shift 2
case "$1" in
  shell)
    case "$2" in
      sha256sum) exit 1 ;;
      pgrep*)
        if [ -f "$d/helpers" ]; then
          cat "$d/helpers"
          if [ -f "$d/polls" ]; then
            n=$(cat "$d/polls")
            if [ "$n" -le 1 ]; then rm -f "$d/helpers"; else echo $((n - 1)) > "$d/polls"; fi
          fi
        fi
        exit 0 ;;
      kill*) rm -f "$d/helpers"; exit 0 ;;
      CLASSPATH=*)
        name=$(printf '%s\n' "$2" | awk '{print $(NF-1)}')
        echo $$ > "$d/helper.pid"
        echo "listening $name"
        exec sleep 1000 ;;
    esac
    exit 0 ;;
  push) exit 0 ;;
  forward)
    if [ "$2" = --list ]; then cat "$d/forwards" 2>/dev/null; exit 0; fi
    if [ "$2" != --remove ]; then sleep "$(cat "$d/forward_delay" 2>/dev/null || echo 0)"; fi
    exit 0 ;;
esac
exit 0
"#;

/// A flapping `adb track-devices`: prints one list and exits at once.
const FLAP_SCRIPT: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$(dirname "$0")/calls.log"
printf '000bdev\tdevice\n'
exit 0
"#;

/// The shared script directory, created (scripts written, closed and renamed into place) on
/// first use. Every test calls this before spawning anything, and `get_or_init` blocks the
/// others until it is done, so no fork in this process can inherit a script's write fd.
fn scripts() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        use std::hash::{Hash, Hasher};
        use std::os::unix::fs::PermissionsExt;
        // Keyed by content, so runs share one directory and nothing accumulates in $TMPDIR.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (ADB_SCRIPT, FLAP_SCRIPT).hash(&mut hasher);
        let dir =
            std::env::temp_dir().join(format!("lantern-fake-adb-bin-{:016x}", hasher.finish()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in [("adb", ADB_SCRIPT), ("adb-flap", FLAP_SCRIPT)] {
            let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
            std::fs::write(&tmp, body).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, dir.join(name)).unwrap();
        }
        dir
    })
}

struct FakeAdb {
    dir: PathBuf,
    program: PathBuf,
    log: PathBuf,
    pidfile: PathBuf,
}

impl FakeAdb {
    /// `forward_delay` makes `adb forward` sleep that long (seconds) before succeeding.
    fn new(tag: &str, forward_delay: &str) -> Self {
        Self::with_script(tag, "adb", forward_delay)
    }

    fn with_script(tag: &str, script: &str, forward_delay: &str) -> Self {
        let shared = scripts();
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
        std::os::unix::fs::symlink(shared.join(script), &program).unwrap();
        std::fs::write(dir.join("forward_delay"), forward_delay).unwrap();
        Self {
            log: dir.join("calls.log"),
            pidfile: dir.join("helper.pid"),
            dir,
            program,
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

/// A SIGKILLed daemon cannot remove its forward; the next session start must remove every
/// `localabstract:lantern-*` forward of its own serial (and nothing else) before adding its own.
#[tokio::test]
async fn start_removes_stale_lantern_forwards_of_its_serial() {
    let fake = FakeAdb::new("stale", "0");
    std::fs::write(
        fake.dir.join("forwards"),
        "fake-serial tcp:1111 localabstract:lantern-0badf00d\n\
         other-serial tcp:2222 localabstract:lantern-12345678\n\
         fake-serial tcp:3333 localabstract:scrcpy\n\
         fake-serial tcp:4444 localabstract:lantern-feedface\n",
    )
    .unwrap();
    let _ = start(&fake.adb()).await;
    let calls = fake.calls();
    let pos = |needle: &str| calls.iter().position(|c| c == needle);
    let stale_1 = pos("-s fake-serial forward --remove tcp:1111");
    let stale_4 = pos("-s fake-serial forward --remove tcp:4444");
    assert!(
        stale_1.is_some() && stale_4.is_some(),
        "stale forwards kept: {calls:#?}"
    );
    for kept in ["tcp:2222", "tcp:3333"] {
        assert!(
            !calls
                .iter()
                .any(|c| c.ends_with(&format!("--remove {kept}"))),
            "removed {kept}: {calls:#?}"
        );
    }
    let created = calls
        .iter()
        .position(|c| c.starts_with("-s fake-serial forward tcp:"))
        .expect("forward never created");
    assert!(
        stale_1.unwrap() < created && stale_4.unwrap() < created,
        "{calls:#?}"
    );
    wait_dead(fake.helper_pid()).await;
}

/// A flapping adb: every `track-devices` client prints one list and exits at once. Decoding a
/// list must not reset the restart backoff (only a client that stayed up >= 5 s does), so the
/// restarts slow down 250 ms -> 500 ms -> 1 s instead of hammering every 250 ms.
#[tokio::test]
async fn flapping_track_devices_backs_off() {
    use futures_util::StreamExt;
    use lantern::adb::{track_devices_with, DeviceEvent};

    let fake = FakeAdb::with_script("flap", "adb-flap", "0");
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

fn position(calls: &[String], pred: impl Fn(&str) -> bool) -> Option<usize> {
    calls.iter().position(|c| pred(c))
}

fn helper_spawned(c: &str) -> bool {
    c.starts_with("-s fake-serial shell CLASSPATH=")
}

/// A previous session's helper (Main or its Cleanup) may still be restoring the phone; the next
/// session must not start until it is gone, or the two race on the stay-awake setting.
#[tokio::test]
async fn start_waits_for_previous_helpers_to_exit() {
    let fake = FakeAdb::new("wait", "0");
    std::fs::write(fake.dir.join("helpers"), "4242\n4243\n").unwrap();
    std::fs::write(fake.dir.join("polls"), "3").unwrap();
    let _ = start(&fake.adb()).await;
    let calls = fake.calls();
    let polls: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.contains(" shell pgrep"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        polls.len(),
        4,
        "3 polls saw helpers, the 4th none: {calls:#?}"
    );
    let spawned = position(&calls, helper_spawned).expect("helper never spawned");
    assert!(polls.iter().all(|&p| p < spawned), "{calls:#?}");
    assert!(
        !calls.iter().any(|c| c.contains(" shell kill")),
        "killed helpers that exited on their own: {calls:#?}"
    );
    wait_dead(fake.helper_pid()).await;
}

/// Helpers still alive after the bound are ours: kill them, then go on (the persisted journal
/// on the phone keeps the true original setting).
#[tokio::test]
async fn previous_helpers_alive_after_the_bound_are_killed() {
    let fake = FakeAdb::new("kill", "0");
    std::fs::write(fake.dir.join("helpers"), "4242\n4243\n").unwrap();
    let t0 = Instant::now();
    wait_for_previous_helpers(&fake.adb(), Duration::from_millis(400))
        .await
        .unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(3),
        "{elapsed:?}"
    );
    let calls = fake.calls();
    let kill = position(&calls, |c| c == "-s fake-serial shell kill -9 4242 4243")
        .unwrap_or_else(|| panic!("helpers not killed: {calls:#?}"));
    let last_poll = calls
        .iter()
        .rposition(|c| c.contains(" shell pgrep"))
        .unwrap();
    assert!(kill < last_poll, "no re-check after the kill: {calls:#?}");
}

#[tokio::test]
async fn no_previous_helpers_means_one_poll() {
    let fake = FakeAdb::new("none", "0");
    wait_for_previous_helpers(&fake.adb(), Duration::from_secs(10))
        .await
        .unwrap();
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert!(calls[0].contains(" shell pgrep"), "{calls:#?}");
}

/// Old helper jars on the phone (other content hashes) are removed at deploy; the current
/// jar and anything else in /data/local/tmp are kept.
#[tokio::test]
async fn start_prunes_old_helper_jars() {
    let fake = FakeAdb::new("prune", "0");
    let _ = start(&fake.adb()).await;
    let calls = fake.calls();
    let push = position(&calls, |c| c.contains(" push ")).expect("no push");
    let remote = calls[push].split_whitespace().last().unwrap().to_owned();
    assert!(
        remote.starts_with("/data/local/tmp/lantern-helper-"),
        "{remote}"
    );
    let prune = position(&calls, |c| c.contains("lantern-helper-*.jar"))
        .unwrap_or_else(|| panic!("old jars not pruned: {calls:#?}"));
    assert!(
        calls[prune].contains(&remote),
        "prune must spare {remote}: {}",
        calls[prune]
    );
    assert!(prune > push, "{calls:#?}");
    wait_dead(fake.helper_pid()).await;
}

/// Concurrent starts in one process must not share the host-side temp copy of the jar.
#[tokio::test]
async fn concurrent_starts_use_distinct_host_jar_paths() {
    let fakes: Vec<FakeAdb> = (0..4)
        .map(|i| FakeAdb::new(&format!("jar{i}"), "0"))
        .collect();
    let starts = fakes.iter().map(|f| {
        let adb = f.adb();
        async move { start(&adb).await.err().map(|e| format!("{e:#}")) }
    });
    let results = futures_util::future::join_all(starts).await;
    let mut locals = Vec::new();
    for (fake, error) in fakes.iter().zip(&results) {
        if let Some(error) = error {
            assert!(error.contains("connect to helper"), "{error}");
        }
        let calls = fake.calls();
        let push = position(&calls, |c| c.contains(" push ")).expect("no push");
        locals.push(calls[push].split_whitespace().nth(3).unwrap().to_owned());
        wait_dead(fake.helper_pid()).await;
    }
    let distinct: std::collections::BTreeSet<_> = locals.iter().collect();
    assert_eq!(
        distinct.len(),
        locals.len(),
        "shared host jar path: {locals:?}"
    );
    for local in &locals {
        assert!(
            !Path::new(local).exists(),
            "host temp jar left behind: {local}"
        );
    }
}
