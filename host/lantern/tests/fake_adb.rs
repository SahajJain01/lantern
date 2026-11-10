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
use lantern::daemon::session::{start, start_with_companion, wait_for_previous_helpers};
use tokio::time::Instant;

/// The main fake. Per-test files (all in the symlink's directory): `calls.log` (every
/// invocation's args), `helper.pid` (pid of the fake helper), `forwards` (`forward --list`
/// output), `forward_delay` (seconds `forward` sleeps), `helpers` (pids `pgrep` reports) and
/// `polls` (after that many `pgrep`s the helpers are gone; absent = never).
///
/// Phone name files: `device_name` (`settings get global device_name`; absent = `null`) and
/// `prop_<name>` (`getprop <name>`; absent = empty).
///
/// Companion / undo-setup files: `pm_path` (`pm path` output; absent = not installed, exit 1),
/// `apk_sha` (`sha256sum` output for a `/data/app/` path), `jar_sha` (`sha256sum` output for
/// any other path; absent = exit 1), `install_results` (one line consumed per `pm install`:
/// printed, exit 0 only for `Success`; absent = `Success`), `uninstall_fail` (present = `pm
/// uninstall` fails) and `forget_fail` (present = the helper's `--forget` fails).
const ADB_SCRIPT: &str = r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\n' "$*" >> "$d/calls.log"
shift 2
case "$1" in
  shell)
    case "$2" in
      sha256sum)
        case "$3" in
          /data/app/*) cat "$d/apk_sha" 2>/dev/null || exit 1 ;;
          *) cat "$d/jar_sha" 2>/dev/null || exit 1 ;;
        esac
        exit 0 ;;
      pm)
        case "$3" in
          path) cat "$d/pm_path" 2>/dev/null || exit 1; exit 0 ;;
          install)
            if [ -s "$d/install_results" ]; then
              r=$(head -n 1 "$d/install_results")
              tail -n +2 "$d/install_results" > "$d/install_results.tmp"
              mv "$d/install_results.tmp" "$d/install_results"
            else
              r=Success
            fi
            echo "$r"
            [ "$r" = Success ] && exit 0
            exit 1 ;;
          uninstall)
            if [ -f "$d/uninstall_fail" ]; then echo 'Failure [DELETE_FAILED_INTERNAL_ERROR]'; exit 1; fi
            rm -f "$d/pm_path"
            echo Success; exit 0 ;;
        esac
        exit 0 ;;
      settings) cat "$d/device_name" 2>/dev/null || echo null; exit 0 ;;
      getprop) cat "$d/prop_$3" 2>/dev/null; exit 0 ;;
      *--forget*)
        if [ -f "$d/forget_fail" ]; then echo 'forget failed: boom' >&2; exit 1; fi
        echo 'forgot journal=restored'; exit 0 ;;
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
    let fut = start(&adb, true);
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
    let error = start(&fake.adb(), true)
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
    let _ = start(&fake.adb(), true).await;
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
    let _ = start(&fake.adb(), true).await;
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
    let _ = start(&fake.adb(), true).await;
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
    // Only once no earlier helper (or its Cleanup) can still be running from an old jar.
    let last_pgrep = calls
        .iter()
        .rposition(|c| c.contains(" shell pgrep"))
        .expect("no pgrep");
    assert!(prune > last_pgrep, "prune before the wait: {calls:#?}");
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
        async move { start(&adb, true).await.err().map(|e| format!("{e:#}")) }
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

// --- companion install ------------------------------------------------------------------------

use lantern::daemon::companion::{self, ensure, record_setup, Install, Setup};

const BASE_APK: &str = "/data/app/~~x==/dev.lantern.companion-y==/base.apk";

fn remote_apk() -> String {
    format!(
        "/data/local/tmp/lantern-companion-{}.apk",
        &companion::companion_sha()[..16]
    )
}

impl FakeAdb {
    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.dir.join(name), body).unwrap();
    }

    /// The companion is installed with an APK whose SHA-256 is `sha`.
    fn installed(&self, sha: &str) {
        self.write("pm_path", &format!("package:{BASE_APK}\n"));
        self.write("apk_sha", &format!("{sha}  {BASE_APK}\n"));
    }
}

fn assert_targets_serial(calls: &[String]) {
    for call in calls {
        assert!(
            call.starts_with("-s fake-serial "),
            "untargeted adb call: {call}"
        );
    }
}

#[tokio::test]
async fn ensure_current_when_sha_matches() {
    let fake = FakeAdb::new("comp-current", "0");
    fake.installed(&companion::companion_sha());
    let got = ensure(&fake.adb()).await.unwrap();
    assert_eq!(got, Install::Current);
    let calls = fake.calls();
    assert_eq!(
        calls,
        vec![
            "-s fake-serial shell pm path --user 0 dev.lantern.companion".to_owned(),
            format!("-s fake-serial shell sha256sum {BASE_APK}"),
        ]
    );
}

#[tokio::test]
async fn ensure_installs_when_missing() {
    let fake = FakeAdb::new("comp-missing", "0");
    let got = ensure(&fake.adb()).await.unwrap();
    assert_eq!(got, Install::Installed);
    let calls = fake.calls();
    assert_targets_serial(&calls);
    let remote = remote_apk();
    let push = position(&calls, |c| c.contains(" push ") && c.ends_with(&remote))
        .unwrap_or_else(|| panic!("apk not pushed to {remote}: {calls:#?}"));
    let install = position(&calls, |c| {
        c == format!("-s fake-serial shell pm install -r -d --user 0 {remote}")
    })
    .unwrap_or_else(|| panic!("no pm install: {calls:#?}"));
    let rm = position(&calls, |c| {
        c == format!("-s fake-serial shell rm -f {remote}")
    })
    .unwrap_or_else(|| panic!("pushed apk not removed: {calls:#?}"));
    assert!(push < install && install < rm, "{calls:#?}");
    // The host-side temp copy is gone too.
    let local = calls[push].split_whitespace().nth(3).unwrap();
    assert!(
        !Path::new(local).exists(),
        "host temp apk left behind: {local}"
    );
}

#[tokio::test]
async fn ensure_updates_an_outdated_companion() {
    let fake = FakeAdb::new("comp-old", "0");
    fake.installed(&"0".repeat(64));
    assert_eq!(ensure(&fake.adb()).await.unwrap(), Install::Updated);
    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains(" pm install -r -d --user 0 ")),
        "{calls:#?}"
    );
    assert!(
        !calls.iter().any(|c| c.contains(" pm uninstall ")),
        "{calls:#?}"
    );
}

#[tokio::test]
async fn ensure_reinstalls_on_incompatible_signature() {
    let fake = FakeAdb::new("comp-incompat", "0");
    fake.installed(&"0".repeat(64));
    fake.write(
        "install_results",
        "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package dev.lantern.companion signatures do not match newer version; ignoring!]\nSuccess\n",
    );
    assert_eq!(ensure(&fake.adb()).await.unwrap(), Install::Updated);
    let calls = fake.calls();
    assert_targets_serial(&calls);
    let installs: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.contains(" pm install "))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(installs.len(), 2, "{calls:#?}");
    let uninstall = position(&calls, |c| {
        c == "-s fake-serial shell pm uninstall --user 0 dev.lantern.companion"
    })
    .unwrap_or_else(|| panic!("no uninstall: {calls:#?}"));
    assert!(
        installs[0] < uninstall && uninstall < installs[1],
        "{calls:#?}"
    );
    let rm = position(&calls, |c| {
        c == format!("-s fake-serial shell rm -f {}", remote_apk())
    })
    .expect("pushed apk not removed");
    assert!(rm > installs[1], "{calls:#?}");
}

#[tokio::test]
async fn ensure_retries_an_incompatible_signature_only_once() {
    let fake = FakeAdb::new("comp-incompat2", "0");
    fake.installed(&"0".repeat(64));
    let incompatible = "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: signatures do not match]\n";
    fake.write("install_results", &incompatible.repeat(3));
    let error = ensure(&fake.adb()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE"),
        "{error:#}"
    );
    let calls = fake.calls();
    assert_eq!(
        calls.iter().filter(|c| c.contains(" pm install ")).count(),
        2,
        "{calls:#?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.contains(" pm uninstall "))
            .count(),
        1,
        "{calls:#?}"
    );
}

#[tokio::test]
async fn ensure_always_removes_pushed_apk() {
    let fake = FakeAdb::new("comp-fail", "0");
    fake.write(
        "install_results",
        "Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]\n",
    );
    let error = ensure(&fake.adb()).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("INSTALL_FAILED_INSUFFICIENT_STORAGE"),
        "{error:#}"
    );
    let calls = fake.calls();
    let install = position(&calls, |c| c.contains(" pm install ")).expect("no install");
    let rm = position(&calls, |c| {
        c == format!("-s fake-serial shell rm -f {}", remote_apk())
    })
    .unwrap_or_else(|| panic!("pushed apk not removed after a failed install: {calls:#?}"));
    assert!(rm > install, "{calls:#?}");
    assert!(
        !calls.iter().any(|c| c.contains(" pm uninstall ")),
        "{calls:#?}"
    );
}

fn state_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "lantern-state-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

type Notes = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

fn recording_setup(state: &Path) -> (Setup, Notes) {
    let notes: Notes = Default::default();
    let sink = notes.clone();
    let setup = Setup::new(Some(state.to_owned()), move |summary: &str, body: &str| {
        sink.lock()
            .unwrap()
            .push((summary.to_owned(), body.to_owned()));
    });
    (setup, notes)
}

#[tokio::test]
async fn setup_notifies_the_first_install_only() {
    let fake = FakeAdb::new("setup-first", "0");
    let state = state_dir("first");
    let (setup, notes) = recording_setup(&state);
    setup.prepare(&fake.adb()).await;
    fake.installed(&companion::companion_sha());
    setup.prepare(&fake.adb()).await;
    let notes = notes.lock().unwrap().clone();
    assert_eq!(
        notes,
        vec![(
            "Lantern set up your phone".to_owned(),
            "Installed the Lantern companion app (keyboard and app list) on fake-serial. \
             Run `lantern undo-setup` to remove it."
                .to_owned()
        )]
    );
    let record = std::fs::read_to_string(state.join("devices/fake-serial.json")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["companion"], serde_json::json!(true));
    assert!(record["since"].as_u64().unwrap() > 1_700_000_000);
    let _ = std::fs::remove_dir_all(&state);
}

/// The phone's name goes into its device record, from the first source that has one:
/// `device_name`, then `ro.product.marketname`, then manufacturer + model.
#[tokio::test]
async fn setup_records_the_phone_name() {
    let fake = FakeAdb::new("setup-name", "0");
    let state = state_dir("name");
    let (setup, _notes) = recording_setup(&state);
    let name = |state: &Path| -> Option<String> {
        let record = std::fs::read_to_string(state.join("devices/fake-serial.json")).unwrap();
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["companion"], serde_json::json!(true));
        record["name"].as_str().map(str::to_owned)
    };
    std::fs::write(fake.dir.join("prop_ro.product.manufacturer"), "Xiaomi\n").unwrap();
    std::fs::write(fake.dir.join("prop_ro.product.model"), "2107113SG\n").unwrap();
    setup.prepare(&fake.adb()).await;
    assert_eq!(name(&state).as_deref(), Some("Xiaomi 2107113SG"));
    std::fs::write(fake.dir.join("prop_ro.product.marketname"), "Mi 11T Pro\n").unwrap();
    setup.prepare(&fake.adb()).await;
    assert_eq!(name(&state).as_deref(), Some("Mi 11T Pro"));
    std::fs::write(fake.dir.join("device_name"), " Xiaomi 11T Pro\r\n").unwrap();
    setup.prepare(&fake.adb()).await;
    assert_eq!(name(&state).as_deref(), Some("Xiaomi 11T Pro"));
    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.contains("shell settings get global device_name")),
        "{calls:#?}"
    );
    let _ = std::fs::remove_dir_all(&state);
}

/// The companion update (`pm install -r`, which kills the companion's process) must not run while
/// a previous session's helper or its Cleanup may still be restoring the keyboard through it:
/// wait for the previous helpers first, then prepare the companion, then start.
#[tokio::test]
async fn start_with_companion_waits_for_previous_helpers_before_the_companion_update() {
    let fake = FakeAdb::new("comp-order", "0");
    fake.installed(&"0".repeat(64)); // outdated: prepare() will install
    std::fs::write(fake.dir.join("helpers"), "4242\n").unwrap();
    std::fs::write(fake.dir.join("polls"), "2").unwrap();
    let state = state_dir("order");
    let (setup, _notes) = recording_setup(&state);
    let _ = start_with_companion(&fake.adb(), &setup, true).await;
    let calls = fake.calls();
    let first_pm = position(&calls, |c| c.contains(" shell pm "))
        .unwrap_or_else(|| panic!("companion never prepared: {calls:#?}"));
    let install = position(&calls, |c| c.contains(" pm install "))
        .unwrap_or_else(|| panic!("no companion update: {calls:#?}"));
    let gone = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.contains(" shell pgrep"))
        .map(|(i, _)| i)
        .nth(2)
        .unwrap_or_else(|| panic!("never saw the helpers gone: {calls:#?}"));
    assert!(
        gone < first_pm && first_pm < install,
        "prepare before the previous helpers exited: {calls:#?}"
    );
    let spawned = position(&calls, helper_spawned).expect("helper never spawned");
    assert!(install < spawned, "{calls:#?}");
    wait_dead(fake.helper_pid()).await;
    let _ = std::fs::remove_dir_all(&state);
}

#[tokio::test]
async fn setup_failure_notifies_once_per_run_and_records_nothing() {
    let fake = FakeAdb::new("setup-fail", "0");
    fake.write(
        "install_results",
        &"Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]\n".repeat(4),
    );
    let state = state_dir("fail");
    let (setup, notes) = recording_setup(&state);
    setup.prepare(&fake.adb()).await;
    setup.prepare(&fake.adb()).await;
    let notes = notes.lock().unwrap().clone();
    assert_eq!(notes.len(), 1, "{notes:#?}");
    assert_eq!(notes[0].0, "Lantern couldn't install its companion app");
    assert!(
        !notes[0].1.contains('\n'),
        "body is the first line: {:?}",
        notes[0].1
    );
    assert!(
        notes[0].1.contains("INSTALL_FAILED_INSUFFICIENT_STORAGE"),
        "{:?}",
        notes[0].1
    );
    assert!(!state.join("devices/fake-serial.json").exists());
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn record_setup_is_true_only_the_first_time() {
    let state = state_dir("record");
    assert!(record_setup(&state, "fake-serial", Install::Installed).unwrap());
    assert!(!record_setup(&state, "fake-serial", Install::Current).unwrap());
    assert!(record_setup(&state, "other", Install::Current).unwrap());
    let _ = std::fs::remove_dir_all(&state);
}

// --- lantern undo-setup -----------------------------------------------------------------------

use lantern::undo;

/// A daemon socket path nobody listens on (lanternd stopped).
fn no_daemon() -> PathBuf {
    state_dir("nosock").join("daemon.sock")
}

async fn run_undo(fake: &FakeAdb, state: &Path) -> (bool, String) {
    let mut out = Vec::new();
    let ok = undo::run(&fake.adb(), &no_daemon(), state, &mut out)
        .await
        .unwrap();
    (ok, String::from_utf8(out).unwrap())
}

fn forget_call(c: &str) -> bool {
    c.starts_with("-s fake-serial shell CLASSPATH=/data/local/tmp/lantern-helper-")
        && c.ends_with(" app_process / dev.lantern.helper.Main --forget")
}

#[tokio::test]
async fn undo_setup_pushes_the_jar_then_forgets_and_uninstalls() {
    let fake = FakeAdb::new("undo", "0");
    fake.installed(&companion::companion_sha());
    let state = state_dir("undo");
    assert!(record_setup(&state, "fake-serial", Install::Installed).unwrap());
    let (ok, out) = run_undo(&fake, &state).await;
    let calls = fake.calls();
    assert!(ok, "{out}\n{calls:#?}");
    assert_targets_serial(&calls);
    // --forget deletes the jar it runs from, so the jar is pushed first when missing.
    let push = position(&calls, |c| {
        c.contains(" push ") && c.contains("lantern-helper-")
    })
    .unwrap_or_else(|| panic!("jar not pushed: {calls:#?}"));
    let forget = position(&calls, forget_call).unwrap_or_else(|| panic!("no --forget: {calls:#?}"));
    let jar = calls[push].split_whitespace().last().unwrap();
    assert!(
        calls[forget].contains(&format!("CLASSPATH={jar} ")),
        "{calls:#?}"
    );
    let uninstall = position(&calls, |c| {
        c == "-s fake-serial shell pm uninstall --user 0 dev.lantern.companion"
    })
    .unwrap_or_else(|| panic!("companion not uninstalled: {calls:#?}"));
    assert!(push < forget && forget < uninstall, "{calls:#?}");
    assert!(
        !state.join("devices/fake-serial.json").exists(),
        "record kept"
    );
    assert_eq!(out.lines().count(), 4, "one line per step: {out}");
    let _ = std::fs::remove_dir_all(&state);
}

#[tokio::test]
async fn undo_setup_reuses_a_present_jar_and_tolerates_a_missing_companion() {
    let fake = FakeAdb::new("undo-present", "0");
    let sha = lantern::daemon::session::helper_sha();
    fake.write(
        "jar_sha",
        &format!("{sha}  /data/local/tmp/lantern-helper-{sha}.jar\n"),
    );
    let state = state_dir("undo-present");
    let (ok, out) = run_undo(&fake, &state).await;
    let calls = fake.calls();
    assert!(ok, "{out}\n{calls:#?}");
    assert!(!calls.iter().any(|c| c.contains(" push ")), "{calls:#?}");
    assert!(calls.iter().any(|c| forget_call(c)), "{calls:#?}");
    assert!(
        !calls.iter().any(|c| c.contains(" pm uninstall ")),
        "nothing to uninstall: {calls:#?}"
    );
    let _ = std::fs::remove_dir_all(&state);
}

#[tokio::test]
async fn undo_setup_failure_exits_nonzero_and_keeps_going() {
    let fake = FakeAdb::new("undo-fail", "0");
    fake.installed(&companion::companion_sha());
    fake.write("forget_fail", "");
    let state = state_dir("undo-fail");
    assert!(record_setup(&state, "fake-serial", Install::Installed).unwrap());
    let (ok, out) = run_undo(&fake, &state).await;
    let calls = fake.calls();
    assert!(!ok, "a failed step must fail the run: {out}");
    assert!(
        calls.iter().any(|c| c.contains(" pm uninstall ")),
        "later steps still run: {calls:#?}"
    );
    assert!(out.contains("forget failed: boom"), "{out}");
    assert_eq!(out.lines().count(), 4, "one line per step: {out}");
    let _ = std::fs::remove_dir_all(&state);
}
