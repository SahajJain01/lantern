# Lantern Many Phones (Plan 4c) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Sahaj's standing instruction for execution:** device steps are DEFERRED to one acceptance run
> at the end (Task 8). Implementers still write and run every host test, the helper build and the
> suite's self-tests, and they write each task's `device_test.py` items. They run no
> `build.py install` and no `device_test.py`, and they report each device step as DEFERRED, never
> as done.

**Goal:** Every checked phone gets its own live session at the same time (G6), the clipboard fans
out across the PC and every live phone without echoes (G7), the unlock prompt works per phone
with one PIN dialog at a time (G8), a PIN-locked phone asks for its PIN as soon as its session is
ready (G9), and the Plan 4b rough edges are fixed.

**Architecture:**
- **Run loop.** `daemon/runs.rs` holds one `PhoneRun` per wanted phone (its start future, live
  `SessionConn`, retry backoff and unplug grace). `drive` polls every run's start and inbound
  frames at once; a failure, retry, offline, Disconnect or Undo touches only that phone's run.
  The connect rules drop "another phone is wanted": a phone that qualifies connects.
- **Hub.** `daemon/ipc.rs` keeps one link per live phone (its helper queue, companion state, lock
  state). One window registry for lanternd's lifetime gives ids unique across phones; a window
  belongs to its phone, and frames only ever travel between a window and its own phone's helper.
- **Clipboard.** `clipboard::ClipFan` keeps one `ClipSync` per live phone plus the phone texts in
  flight to the PC. A PC copy goes to every live phone; a phone copy is set on the PC and its echo
  fans out to the other phones, never back to its source.
- **Unlock.** Each phone has its own lock stretch, notification and Unlock row; one PIN dialog at a
  time across phones; the answer stays pinned to its phone. The helper reports
  `locked{reason:"session"}` at session-up when its PIN keyguard is locked (G9).
- **Suite.** `tools/device_test.py` gains per-serial readers, a two-phone harness (`--pair`) that
  restores both phones, and manual items that print every step first and wait on observed device
  and journal state instead of stdin.

**Tech Stack:** Rust (tokio, zbus 5, serde_json: all existing; no new crate), Java helper (host-JVM
tests), Python 3 for `tools/device_test.py` (+ `unittest` self-tests). No companion change.

**Spec:** `docs/superpowers/specs/2026-11-14-lantern-multi-phone-design.md` (approved), section
"Several sessions (G6–G8), Plan 4c" and "Addendum (2026-11-15): Plan 4c scope" (G9 and the Plan 4b
rough edges). Read `docs/HANDOFF.md` ("Plan 4b decisions", "Known minor issues") and
`docs/protocol.md` first.

**Out of scope:** moving a window between phones; per-phone autoconnect; Wi-Fi adb; a settings
window; a cap on phones (two is the tested case); the other Known minor issues not named below.

## Decisions made in this plan (the design left them to the builder; Sahaj may override)

1. **Window ids** come from one registry for lanternd's lifetime: unique across phones and never
   reused within a run (protocol.md's "ids start over with the next session" changes).
   Single-instance focus keys on `serial|pkg:user`.
2. **`lantern status` top level** describes the *default phone*: the phone whose session came up
   most recently in this run. Else `starting` for the first phone in view order whose start is in
   flight, else `paused`, else `idle`. `windows[]` entries gain an optional `phone`.
3. **An `open` without `phone`** (old pins, hand-typed commands) goes to the default phone, with
   Plan 3's "app not on this phone" check against it.
4. **Connect rules:** `on_plug` loses its `wanted` argument. A `live` phone connects, and so does a
   phone with autoconnect on and not `off`, whatever else is live. Plan 4b decision 4 ("prompt
   when another phone is live") and the tray's row 15 ("Lantern connects one phone at a time")
   are retired; id 15 is never reused.
5. **Pending connect-and-open requests are independent per phone.** A choice for one phone no
   longer drops another phone's request (there is no switch-away any more). Disconnect and Undo
   still drop that phone's own request.
6. **Undo:** one undo at a time. Commands for other phones run normally during it. A Connect for
   the phone being undone, and an Undo of a second phone, are refused with a notification saying
   why (fixes the Plan 4b "Connect click during Undo is dropped silently" minor).
7. **Clipboard fan-out goes through the PC selection** (design): a phone copy reaches the other
   phones when its own PC selection event comes back. Without a PC clipboard (no Wayland) phone
   copies reach no other phone. Lantern's phone texts in flight to the PC (at most 8) make a stale
   echo go nowhere and keep a burst from echoing a copy back to its source.
8. **Fan-out sends** `try_send` to every live helper first; helpers whose queue is full get one
   shared 1 s wait. A wedged helper ends only its own session.
9. **G9 is helper-side:** at session-up the helper reads its lock on its `lantern-clip` thread and,
   if sure it is locked, opens a stretch with the new `locked{reason:"session"}`. lanternd shows
   the same "Unlock your {name}" notice. A swipe phone reads `deviceLocked=0` and never prompts.
   Each new helper session checks once.
10. **Unlock dialogs:** a click (notification or tray) for phone B while A's dialog is open is
    queued, at most one per phone, and B's dialog opens when A's closes if B is still in a stretch.
11. **A PIN is sent only to a phone whose session is up and whose lock stretch is open**
    (`Hub::send_unlock`). This fixes the Plan 4a minor "a PIN can still be sent after the
    stretch ended". From Task 4 on, the prompt pins a dialog to its stretch's serial, never to
    "the live phone".
12. **`adb forward tcp:0`:** each session takes the port adb chose. This removes the free-port race
    that two concurrent starts would make likelier.
13. **The companion-install failure notice** is once per phone per run (was once per run).
14. **The start-up stale-live check** runs once adb's first device list was handled and the grace
    passed, or at the latest grace + 30 s after start (`Timing.list_wait`). This fixes the Plan 4b
    minor "waits forever without a first list".
15. **Notices on exit:** when lanternd stops (Quit or a signal), the connect and unlock prompts close
    every notification they show. lanternd waits at most 3 s for that.
16. **kdialog texts** (the Remove confirm and the PIN dialog) are `<qt>…</qt>` with the phone name
    escaped, so Qt always reads them as rich text.
17. **A `menu.json` rerun** picks as primary the serial that owns the most per-phone entries at
    legacy (unsuffixed) stems, before the pin/entry rules.
18. **A replug keeps an offered app:** a `PluggedIn` that replaces a shown "Connect your {name}"
    keeps its app. Its body names the app, and its Connect opens that app.
19. **Tray tooltip** with several live phones: `Connected to {a} and {b}`, or
    `Connected to {n} phones` for three or more. A live phone that isn't ready shows first
    (`Connecting to {name}…` / `Reconnecting to {name}…`).
20. **Suite:** `--pair` runs the two-phone items. The second phone is `LANTERN_OTHER_SERIAL`, else
    the only other attached phone. Manual waits last `LANTERN_MANUAL_WAIT` (default 3600 s), and
    the suite never reads stdin.

## Global Constraints

- Phones: Xiaomi 11T Pro `20e9318` (swipe), Motorola edge 50 pro `ZD222LMDFV` (PIN, work profile
  user 10). No root.
- **Protocol stays v1 between helper and lanternd.**
  - New on the helper link: only the `locked.reason` value `"session"`, with the new golden
    `control_locked_session.bin`.
  - Host-only (window IPC): the optional `windows[].phone` in `status_report`.
  - The window→lanternd `open` keeps its optional `phone`.
  - `git diff --stat host/proto/golden` may show only the new file; every existing golden is
    byte-identical.
- **Every adb call names its serial (`adb -s S`) and has a timeout**, the new `forward tcp:0` and
  every suite call included. `track-devices` is the only serial-less call.
- **Each phone keeps its own** helper, adb forward, journal, retry backoff, companion state, lock
  stretch, clipboard echo memory and restore.
- **Anti-goal:** one phone's failure, retry, offline, Disconnect or Undo never closes, restarts or
  sends frames to another phone's session or windows. Pin it with a test wherever a task touches
  that path.
- `LANTERN_SERIAL` keeps its meaning: lanternd serves only that phone and ignores every other
  serial.
- **Values:**
  - `autoconnect` default `false`.
  - Unplug grace 10 s (`LANTERN_UNPLUG_GRACE_MS` override).
  - Pending request 120 s; first-list cap 30 s.
  - Helper send wait 1 s.
  - Phone texts in flight at most 8.
  - Shutdown notice budget 3 s.
  - Notification calls 5 s each.
  - Suite manual wait 3600 s.
- **Never log clipboard text, typed text or a PIN** (lengths only). Plan 4a's PIN rules hold:
  `Pin`/`SecretPin`, no PIN in argv, a PIN goes only to the phone it was asked for.
- **Notifications:** titles are plain text; names and app names in bodies are escaped
  (`notices::escape_markup`). At most one notice per phone per prompt, replaced by id.
- **Host test isolation (binding, every task).** Every `lantern` process a test spawns past
  argument parsing gets:
  - scratch `XDG_DATA_HOME`/`XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_CACHE_HOME`;
  - `DBUS_SESSION_BUS_ADDRESS` pointing at a dead socket;
  - stubbed `kbuildsycoca6`, `notify-send`, `kdialog` (and `adb`) first on PATH. Each stub writes
    a marker that the test asserts.
  
  Bus tests use a private `dbus-daemon` with a config that has no `<servicedir>` (`tests/common`
  `Bus`) and a fake `org.freedesktop.Notifications`. Spawned daemons are stopped by drop guards.
  In-process tests never call `Dirs::from_env()`/`companion::state_dir()`. Python self-tests set
  `TMPDIR` and `HOME`-derived paths to scratch and never call `adb`, `systemctl` or `busctl`.
- **Fingerprint check (every implementer, every fix round)**, before and after a full
  `cargo test --workspace --offline`:
  ```sh
  find ~/.local/share/applications ~/.local/share/lantern ~/.local/share/desktop-directories \
       ~/.config/menus ~/.config/lantern ~/.local/state/lantern -printf '%p %T@ %s\n' 2>/dev/null | sort > fp
  ls ~/.cache | grep ksycoca6 >> fp
  ```
  The two must be identical. Also `ps -eo pid,args | grep target/debug/lantern` must be empty, and
  `systemctl --user show-environment | grep LANTERN` must be empty.

## Review Focus

1. **A frame crossing phones when both have the same app open.** A helper's frame for window N, or
   a window's input, must never reach another phone's window or helper, even with the same
   pkg/user on both. Pinned by Task 4 `same_app_on_two_phones_is_two_windows` and
   `helper_frames_only_reach_their_own_phones_windows`.
2. **A clipboard burst.** Both phones copy before the first copy's PC echo is back. A phone must
   never get its own copy back, and a stale echo must never overwrite the newer copy. Pinned by
   Task 4 `burst_never_echoes_to_source` and Task 5 `phone_copy_reaches_pc_and_other_phone_only`.
3. **A PIN reaching the wrong phone.** Phone B locks while A's dialog is open, or A unlocks by
   itself before the answer. Expected: the answer goes to A only, and only while A's stretch is
   open; otherwise it is wiped. Pinned by Task 6 `answer_goes_to_the_phone_asked_for` and
   `click_for_b_while_a_asks_is_queued`, and Task 5 `pin_for_an_unlocked_phone_is_dropped`.
4. **One phone's trouble disturbing the other.** Helper death, a cable pull, Disconnect or Undo on
   one phone must leave the other's helper connection, windows and status untouched. Pinned by
   Task 5 `helper_death_of_one_phone_keeps_the_other`, `offline_of_one_phone_keeps_the_other`,
   `disconnect_one_keeps_the_other` and `undo_of_one_keeps_the_others_session_and_windows`.
5. **Stop or Quit with two live phones.** Neither phone may be left changed, and the suite's
   two-phone pass must not leave the second phone or its record changed. Expected: every session
   gets `bye`, flags behave as with one phone, and the harness restores both phones and both
   records. Pinned by Task 5 `stop_ends_every_session_and_keeps_flags` and
   `quit_clears_live_of_every_phone`, and Task 1 `pair_cleanup_runs_every_step_in_order`.

---

## File Structure

```
tools/device_test.py                       MODIFY  per-serial readers, sleep-tolerant release, pair harness,
                                                   no stdin waits, items 22 and 27-34
tools/test_device_test.py                  NEW     unittest self-tests for the suite's pure helpers
android/helper/src/dev/lantern/helper/{LockWatch,Main}.java      MODIFY  G9 session-up check
android/helper/test/dev/lantern/helper/{LockWatchTest,ProtoTest}.java  MODIFY
host/proto/src/lib.rs, host/proto/tests/golden.rs, host/proto/golden/control_locked_session.bin  MODIFY/NEW
host/lantern/src/daemon/catalog.rs         MODIFY  per-phone sync windows; primary on a menu.json rerun
host/lantern/src/adb.rs, daemon/session.rs MODIFY  forward tcp:0, SessionConn::poll_next
host/lantern/src/daemon/companion.rs       MODIFY  failure notice per phone
host/lantern/src/clipboard/mod.rs          MODIFY  ClipFan
host/lantern/src/daemon/{ipc,registry}.rs  MODIFY  per-phone links, window registry, routing, fan-out
host/lantern/src/daemon/ipc_phones_tests.rs NEW    Hub tests with two phones (included from ipc.rs)
host/lantern/src/daemon/runs.rs            NEW     PhoneRun / Runs: every wanted phone's session
host/lantern/src/daemon/{mod,phones,control}.rs  MODIFY  several sessions, connect rules, commands
host/lantern/src/unlock.rs, tray/{menu,mod}.rs   MODIFY  per-phone stretches, dialog queue, tray
host/lantern/src/{connect,notices,notify}.rs, window/mod.rs   MODIFY  rough edges (notices)
host/lantern/tests/{ipc,unlock,tray,connect,fake_adb,cli}.rs  MODIFY
docs/protocol.md, README.md, docs/HANDOFF.md      MODIFY
```

Test commands:
- Host: `cd host && export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace --offline`
- Lint: `cargo clippy --workspace --offline --all-targets -- -D warnings`
- Helper (Task 2; a fresh worktree needs both builds before `cargo test`):
  `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py all`
- Suite self-tests: `TMPDIR=$(mktemp -d) python3 -m unittest tools/test_device_test.py -v`
- Suite syntax: `python3 -m py_compile tools/device_test.py && python3 tools/device_test.py --help`
- Install and device run (Task 8 only): `python3 tools/build.py install && python3 tools/device_test.py …`

---

### Task 1: Device suite: per-serial readers, sleep-tolerant release, pair harness, no stdin waits

**Files:**
- Modify: `tools/device_test.py`
- Create: `tools/test_device_test.py`

**Interfaces (Produces):** later tasks write their items on these.
- `adb(*args, timeout=20, serial=None)`: always `adb -s (serial or SERIAL)`.
- `phone_state(serial=None) -> dict`: the existing keys plus `wakefulness` (from `dumpsys power`
  `mWakefulness=`, else `"?"`). `parse_wakefulness(text) -> str` is pure.
- `released(st) -> bool` (pure):
  - true when `displays == ["0"]`, there are no forwards and no helper pids;
  - and `panel == "On"` **or** `wakefulness != "Awake"`, so a phone asleep on its own timeout is
    released.
- `restored(st, serial=None) -> bool`: the same panel rule, against `baseline(serial)`.
- `BASELINES: dict[str, dict]` replaces the `STAY_ON_RESTORED`/`IME_RESTORED`/`IMES_RESTORED`
  globals. The `LANTERN_STAY_ON_RESTORED` env override still applies to `SERIAL`.
  - `baseline(serial=None) -> {stay_on, ime, imes}`.
  - `read_baseline(serial)`: reads with no session on that phone. It requires `released(...)` for
    that serial, and stops lanternd only when lanternd serves that serial.
- `status()` turns `subprocess.TimeoutExpired` into `Fail`.
- `wait_journal(pattern, since, timeout) -> (line, secs)`: polls
  `journalctl --user -u lanternd --since <since> -o cat` for a regex. Lines are never printed in
  full: only the match.
- `MANUAL_WAIT = int(os.environ.get("LANTERN_MANUAL_WAIT", "3600"))`.
- `STEPS: dict[str, list[str]]`: the person's steps for every manual item, in order.
  `manual_plan(items) -> list[str]` (pure): one header line per manual item, then its numbered
  steps. `main()` prints it for every selected manual item before running any item, and each
  manual item prints its own steps again at its start with `ask()`.
- No `input(` call anywhere in the file. Every wait for a person waits on an observed edge (adb
  state, `status()`, the journal, logcat, `dumpsys trust`) for `MANUAL_WAIT`.
- `Window(pkg, tag, debug_input=None, phone=None)`: `phone` adds `--phone S`.
- `app_id_of(pkg, serial=None)` and `entry_for(...)` take the serial.
- **Pair harness:**
  - `OTHER`: `LANTERN_OTHER_SERIAL`, else `pick_other(devices_text, SERIAL)` (pure). That is the
    only attached `device` other than `SERIAL`. `Fail` lists them when there are none or several.
  - `PAIR: set[str]` of item names, which run only with `--pair`. A run with `--pair` selects only
    `PAIR` items.
  - `class two_phones` (context manager). It refuses before changing anything unless exactly
    `{SERIAL, OTHER}` are attached. `__enter__`, in order:
    1. Save the PC clipboard text.
    2. Save `settings.toml`, then set `autoconnect = false`.
    3. Save both records' `off`/`live`.
    4. Read `baseline(OTHER)` and `baseline(SERIAL)`.
    5. `manager_env(unset=["LANTERN_SERIAL"])`.
    6. `wait_daemon`, then `lantern connect SERIAL` and `lantern connect OTHER`.
    7. Wait until both `phone_status(s)["state"] == "ready"` (60 s).
    8. For each phone whose `dumpsys trust` current-user line says `deviceLocked=1`:
       `ask("Unlock <name>: click Unlock on its notification and type its PIN")`, then wait
       `MANUAL_WAIT` for `deviceLocked=0`. Without `--manual` this is a `Fail` naming the phone.
  - `__exit__` runs these steps through `cleanup()`, each guarded, in this order:
    1. Restore the PC clipboard.
    2. Restore `settings.toml`.
    3. `systemctl stop` lanternd.
    4. Wait for `released` + `restored` on each phone (20 s each).
    5. Write both records' saved `off`/`live` back under `fcntl.flock(devices/.lock)` (temp +
       rename; a key that was absent is deleted).
    6. Run the `manager_env` exit, which restores `LANTERN_SERIAL` and restarts.
    7. `wait_daemon`, then `wait_ready(20)` for `SERIAL`.
    
    `pair_cleanup_steps(...) -> list[(name, fn)]` builds the list, so the order is testable.
- Items 8 and 14 are rewritten on the above:
  1. Print the steps.
  2. Take `t0 = phone_now()`.
  3. Wait for the phone off adb, then `status()["state"] == "idle"`, then back as `device` (each
     `MANUAL_WAIT`).
  4. Item 8 checks `cleanup: panel on` in `logcat_since(t0, ["lantern"])`, not the whole buffer,
     and no y/n question.
  5. Their cleanup's first step waits for the phone back on adb (`MANUAL_WAIT`) before restoring
     anything.

- [ ] **Step 1: Failing self-tests** (`tools/test_device_test.py`, `unittest`; it imports the module
  with `TMPDIR` set to scratch and monkeypatches `sh`, so no device or desktop tool runs)
  - `test_released_accepts_a_sleeping_phone`:
    - `{"panel":"Off","wakefulness":"Asleep","displays":["0"],"forwards":[],"helper_pids":[]}` is
      released; so is `"Dozing"`.
    - `"Awake"` with panel Off is not.
    - `displays ["0","12"]` is not.
  - `test_restored_uses_the_same_panel_rule`, with `BASELINES` injected.
  - `test_parse_wakefulness`: `"  mWakefulness=Asleep\n"` gives `"Asleep"`; no line gives `"?"`.
  - `test_pick_other`: the `adb devices` text `"List…\n20e9318\tdevice\nZD222LMDFV\tdevice\n"`
    gives `ZD222LMDFV`. An `unauthorized` or `offline` phone is ignored. None, or two others,
    raises `Fail`.
  - `test_manual_plan_lists_every_step_up_front`: for `["usb_yank", "unlock_prompt"]` the output
    holds both headers and every `STEPS` line in order. A non-manual item adds nothing.
  - `test_every_manual_item_has_steps`: `set(STEPS) == MANUAL`.
  - `test_no_stdin_waits`: the module source has no `input(`.
  - `test_pair_cleanup_runs_every_step_in_order`: with recording fakes and the second step
    raising, all seven names run, in the order above, and `cleanup(ok=False, …)` doesn't raise.
- [ ] **Step 2:** Run `TMPDIR=$(mktemp -d) python3 -m unittest tools/test_device_test.py -v`.
  Expected: FAIL (missing names).
- [ ] **Step 3:** Implement the interfaces above.
  - Every existing caller of `released`/`restored`/the baseline globals moves to the new names.
  - The docstring's item list and its "900 s" wording are updated.
  - `--pair` and `LANTERN_OTHER_SERIAL` are documented in the docstring and `--help`.
- [ ] **Step 4:** Run the self-tests, then `python3 -m py_compile tools/device_test.py && python3
  tools/device_test.py --help`. Expected: PASS, and `--help` lists `--pair`.
- [ ] **Step 5: Device check** (DEFERRED to Task 8): items 8 and 14 in their new form, run
  together with the other manual items.
- [ ] **Step 6:** Commit with `test(device): per-serial readers, sleeping phones count as released, two-phone harness, manual steps up front without stdin waits`.

### Task 2: G9: the unlock prompt at connect

**Files:**
- Modify: `host/proto/src/lib.rs`, `host/proto/tests/golden.rs`; create
  `host/proto/golden/control_locked_session.bin`
- Modify: `android/helper/src/dev/lantern/helper/LockWatch.java`, `Main.java`
- Test: `android/helper/test/dev/lantern/helper/LockWatchTest.java`, `ProtoTest.java`,
  `host/lantern/tests/unlock.rs`
- Modify: `tools/device_test.py` (item 22, new item 27), `docs/protocol.md` (Lock rules)

**Interfaces (Produces):**
- Proto: `LockReason::Session`, serialized as `"session"`. The golden
  `control_locked_session.bin` is kind Control, stream 0, `{"t":"locked","reason":"session"}`,
  listed at the top of `golden.rs`.
- Java:
  - `LockWatch.checkNow(String reason)`. It reads `lock.read()` once and calls
    `onLocked(reason)` only on `Boolean.TRUE`. An unknown or false read does nothing, and so
    does a call inside a stretch or after `close()`.
  - `Main`'s `onSessionUp` hook, right after `phoneClip.start(clipSync)`, calls
    `phoneClip.post(() -> lockWatch.checkNow("session"))`, so `dumpsys` never runs on the
    control thread.
- lanternd: no code change. `Locked{Session}` opens a stretch like any reason, and the notice text
  is unchanged ("Unlock your {name}", "Lantern can't paste or open apps while {name} is locked.").

- [ ] **Step 1: Failing tests**
  - Proto:
    - `locked_session_json_shape`: the encoding is `{"t":"locked","reason":"session"}` and it
      round-trips;
    - `golden_vectors` lists `control_locked_session`;
    - the existing `control_locked.bin` is unchanged.
  - `ProtoTest.goldenLockedSessionMatchesRust`: the reason reads `"session"`.
  - `LockWatchTest`:
    - `checkNowLockedStartsAStretch`: a `dump(1)` source gives exactly one `locked{session}`;
      a second `checkNow` sends nothing.
    - `checkNowUnlockedOrUnknownSendsNothing`: `dump(0)` and an empty dump both send nothing.
    - `checkNowAfterCloseSendsNothing`.
  - `tests/unlock.rs` `session_lock_prompts_at_once`: `LockEvent::Locked(LockReason::Session)`
    gives one `Notify` titled `Unlock your Pixel` with the `unlock` action.
- [ ] **Step 2:** Run the host tests and the helper build. Expected: FAIL.
- [ ] **Step 3:** Implement. Write the golden with `LANTERN_WRITE_GOLDEN=1 cargo test -p
  lantern-proto --test golden`, then check that `git diff --stat host/proto/golden` shows only the
  new file.
- [ ] **Step 4:** Run `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py all`, the
  host tests and clippy. Expected: PASS.
- [ ] **Step 5: Docs.** In protocol.md's "Lock rules", `locked.reason` gains `"session"`: sent
  once per helper session at session-up when a sure read says locked. Update the `t` table row.
- [ ] **Step 6: Device items** (DEFERRED to Task 8):
  - Item 22 `swipe_phone_never_prompts` gains a first step, inside its Notify monitor:
    `systemctl restart` lanternd, `wait_ready(15)`, then no `Unlock …` Notify and no `lock: phone
    locked` in `logcat_since` for 10 s after ready.
  - New item 27 `unlock_prompt_at_connect` (in `MANUAL`, Motorola as `SERIAL`; it checks
    `SERIAL == "ZD222LMDFV"` as item 21 does). `STEPS`:
    "1. Leave the Motorola locked (the suite locks it). 2. When 'Unlock your edge 50 pro' shows,
    click Enter PIN… and type the PIN. 3. Then leave the phone alone for 5 minutes."
    1. Save `settings.toml` and set `screen_off = true`.
    2. Lock the phone: `adb shell input keyevent KEYCODE_SLEEP`, then wait for `deviceLocked=1`
       (15 s).
    3. Start a `NotifyMonitor`, take `t0 = phone_now()`, then `systemctl restart` lanternd and
       `wait_ready(20)`.
    4. Within 10 s of ready, with no copy and no open, there is a Notify `Unlock your
       <device_name>`, and logcat since `t0` has `lock: phone locked (session)`.
    5. Wait `MANUAL_WAIT` for `deviceLocked=0` and the journal's `unlock result ok=true`.
    6. Every 30 s for 5 minutes: `deviceLocked=0`, `status()` still ready with the same helper
       sha, and the panel `Off`.
    7. Cleanup: restore `settings.toml`, restart lanternd, `wait_ready`.
- [ ] **Step 7:** Commit with `feat: a PIN-locked phone asks for its PIN as soon as its session is ready (locked{reason:"session"})`.

### Task 3: Per-phone building blocks: catalogue sync, adb forward, companion notice, primary on rerun

**Files:**
- Modify: `host/lantern/src/daemon/catalog.rs`, `daemon/ipc.rs` (call sites only),
  `host/lantern/src/adb.rs`, `daemon/session.rs`, `daemon/companion.rs`
- Test: `catalog.rs` units, `adb_tests.rs`, `tests/fake_adb.rs`, `companion.rs` units

**Interfaces (Produces):**
- `Catalog` keeps one sync window per phone. It holds a `HashMap<String, PhoneSync>` instead of
  the single `serial/blocked/menu_ok/synced/seen/users/keep` fields. Each method takes the phone:
  ```rust
  pub fn start_session(&mut self, serial: &str);          // opens (or reopens) that phone's window
  pub fn end_session(&mut self, serial: &str);            // forgets that phone's window only
  pub fn synced(&self, serial: &str) -> bool;
  pub fn upsert(&mut self, serial: &str, e: &AppEntry) -> Result<()>;
  pub fn remove(&mut self, serial: &str, pkg: &str, user: u32) -> Result<()>;
  pub fn end_sync(&mut self, serial: &str, count: u32) -> Result<usize>;
  ```
  - A call for a serial with no open window is logged and ignored (`Ok`).
  - The Hub passes its single session serial in this task (Task 4 passes each link's).
- `choose_primary(legacy, pins, owners: &[(String /*stem*/, String /*serial*/)]) -> Option<String>`.
  - `owners` are the owned files already in the per-phone format at an unsuffixed stem: their
    `Categories=` is exactly `X-Lantern-<tag(serial)>;` and their stem is `app_id(pkg, user)`.
  - When `owners` is non-empty, the serial with the most of them wins (ties: the smallest
    serial). Otherwise the existing pins → entries → smallest rule applies.
  - `migrate` builds `owners`.
- `Adb::forward_any(&self, abstract_name: &str) -> Result<u16>`: `adb -s S forward tcp:0
  localabstract:<name>` with `ADB_CALL_TIMEOUT`, parsed by the pure `parse_forward_port(out: &str)
  -> Option<u16>` (first line trimmed, `1..=65535`).
  - `session::start` uses it instead of binding a local listener for a free port.
  - `PendingSession.forward` is set right after the call returns. A future cancelled mid-call
    leaves no port to remove: `remove_stale_lantern_forwards` at the next start cleans it.
- `companion::Setup`: `failure_notified: Mutex<HashSet<String>>`, so the failure notice is once
  per serial per run.

- [ ] **Step 1: Failing tests**
  - `catalog.rs`:
    - `two_phones_sync_at_once`: `start_session("A")`, `start_session("B")`; then `upsert("A",
      com.a)`, `upsert("B", com.b)`, `upsert("A", com.c)`, then `end_sync("A", 2)`. Expected:
      `synced("A")`, `!synced("B")`, B's entries untouched, and an A-only stale entry pruned.
      Then `end_sync("B", 1)` prunes only B's.
    - `end_session_forgets_only_that_phone`: after `end_session("A")`, `upsert("A", …)` writes
      nothing and B's sync goes on.
    - `rerun_after_lost_menu_json_keeps_the_primary`: migrate legacy A;B files with pins making
      B primary; delete `menu.json`; migrate with `pins = None`. Expected: primary is still `B`,
      the set of file names and their bytes are identical to after the first run, and there is no
      `-pB` file.
    - `choose_primary_prefers_existing_owners`: owners `[("lantern.com.a","B")]` beat pins naming
      A.
  - `adb_tests.rs`:
    - `parse_forward_port`: `"41234\n"` gives 41234; `""`, `"0"` and `"x"` give None;
    - `stale_forwards_are_per_serial`: a list with both phones' `lantern-*` forwards gives only
      the asked serial's.
  - `tests/fake_adb.rs` `start_uses_the_port_adb_chose`: the fake adb answers `forward tcp:0 …`
    with a port it listens on. The session connects there, and the fake's log shows `-s <serial>
    forward tcp:0`.
  - `companion.rs` `failure_notice_once_per_phone`: two failing `prepare`s for `a` give one
    notice; one for `b` gives a second.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Keep `phones_with`, `app_name` and the folder logic as they are.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS; the fingerprint is identical.
- [ ] **Step 5: Device check:** none of its own. Pass A's items 1, 7 and 10 (Task 8) cover the
  forward and the catalogue.
- [ ] **Step 6:** Commit with `refactor: per-phone catalogue sync windows, adb-chosen forward ports, per-phone companion notice; a menu.json rerun keeps the primary`.

### Task 4: The hub serves several phones: links, window registry, routing, clipboard fan-out

**Files:**
- Modify: `host/lantern/src/daemon/ipc.rs`, `daemon/registry.rs`, `clipboard/mod.rs`,
  `host/proto/src/lib.rs` (`WindowInfo.phone`), `daemon/mod.rs` (call sites, still one session),
  `unlock.rs` (`LockEvent` with serials; the dialog pinned to the stretch's serial)
- Create: `host/lantern/src/daemon/ipc_phones_tests.rs` (`#[cfg(test)] #[path =
  "ipc_phones_tests.rs"] mod phones_tests;` in `ipc.rs`)
- Test: `clipboard/mod.rs` units, `ipc_phones_tests.rs`, `tests/ipc.rs`, `tests/unlock.rs`,
  `tests/tray.rs` (adapted)

**Interfaces (Produces):**
- `clipboard::ClipFan`:
  ```rust
  pub const IN_FLIGHT_MAX: usize = 8;
  #[derive(Debug, Default)] pub struct ClipFan { /* HashMap<String, ClipSync>, VecDeque<String> */ }
  impl ClipFan {
      pub fn add(&mut self, serial: &str);       // session up: a fresh ClipSync
      pub fn remove(&mut self, serial: &str);    // session down
      pub fn reset(&mut self);                   // sync back on: every memory and the in-flight list
      pub fn from_phone(&mut self, serial: &str, text: String) -> String; // set this on the PC
      pub fn pc_changed(&mut self, offer: &PcOffer) -> Vec<(String, String)>; // (serial, text) to send
  }
  ```
  `pc_changed` rules, in order:
  1. If the offer's text equals in-flight entry *i*, drop entries `0..=i`. If a newer entry
     remained after *i*, return nothing: a stale echo, and no phone's memory changes.
  2. Otherwise, or for the newest entry, ask every phone's `ClipSync::pc_changed` in serial order
     and return the ones that send.
  
  An unsyncable offer (secret, empty, none, oversize) reaches every `ClipSync`, which clears it;
  the in-flight list is unchanged. `from_phone` pushes onto the list and drops the oldest past
  `IN_FLIGHT_MAX`.
- `registry::Registry`: keys `"{serial}|{pkg}:{user}"`. It is never reset, and ids are never
  reused within the run.
- Proto: `WindowInfo { id, pkg, user, #[serde(default, skip_serializing_if = "Option::is_none")]
  phone: Option<String> }`. The hub always fills it in.
- `ipc`:
  ```rust
  #[derive(Debug, Clone, PartialEq, Eq)] pub struct HelperWedged { pub serial: String }
  impl Hub {
      pub fn session_up(&mut self, serial: &str, helper: mpsc::Sender<Frame>, helper_sha: &str);
      pub fn session_down(&mut self, serial: &str);
      pub fn default_phone(&self) -> Option<&str>;     // the link that came up last
      pub fn live_serials(&self) -> Vec<String>;       // links, in the order they came up
      pub fn set_paused(&mut self, serial: Option<String>);
      pub fn locked(&self, serial: &str) -> bool;
      pub fn companion(&self, serial: &str) -> Option<(CompanionState, Option<String>)>;
      pub async fn on_client(&mut self, event: ClientEvent) -> Result<(), HelperWedged>;
      pub async fn on_helper(&mut self, serial: &str, frame: Frame) -> Result<(), HelperWedged>;
      pub async fn on_pc_clip(&mut self, offer: PcOffer) -> Vec<HelperWedged>;
      /// false: not sent (no link, or its lock stretch isn't open); the PIN is wiped.
      pub async fn send_unlock(&mut self, serial: &str, pin: Pin) -> Result<bool, HelperWedged>;
  }
  ```
  `set_session`, `reset` and `session_started` are removed. Their replacements:
  - **`session_up`** adds a link (helper queue, sha, `companion: None`, `locked: false`), calls
    `catalog.start_session(serial)` and `clip.add(serial)`.
  - **`session_down`** finishes only that phone's window clients and releases their ids, then
    `catalog.end_session` and `clip.remove`. If the phone was locked it sends
    `LockEvent::Unlocked{serial}`. It keeps that phone's reopens and leaves every other phone's
    state alone.
  - **Windows** map `win → (cid, serial)`.
  - **`on_open`** routes by `phone`: to that link if it is up, otherwise
    `refuse_for_phone` as today. Without `phone` it goes to `default_phone()`, with
    `not_on_this_phone` checked against it (`catalog.synced(serial)`).
  - **`on_helper(serial, …)`** for stream N reaches window N's client only when N belongs to
    `serial`. Otherwise it is dropped with a `debug` log (stream, serial), never forwarded.
  - **Client frames** go only to their window's phone's helper.
  - **Clipboard.** A phone `clip` goes to `out.set(clip.from_phone(serial, text))`.
    `on_pc_clip` sends `clip.pc_changed(&offer)` to each link: `try_send` first, then one shared
    `HELPER_SEND_TIMEOUT` wait (`join_all`) for full queues. Each one still full returns its
    `HelperWedged`.
  - **Clipboard sync turned back on** calls `clip.reset()`.
  - **`status()`** top level (decision 2):
    1. `default_phone()` as `ready`, with its sha and companion;
    2. else the first view phone with `PhoneState::Starting` as `starting`;
    3. else `paused` with the `set_paused` serial;
    4. else `idle`.
    
    Windows are sorted by id, with `phone`.
- `unlock::LockEvent`:
  ```rust
  pub enum LockEvent {
      Locked { serial: String, reason: LockReason },
      Unlocked { serial: String },
      Result { serial: String, ok: bool, reason: Option<String> },
      Reopen { serial: String, apps: Vec<(String, u32)> },
  }
  ```
  In this task the prompt still runs one stretch at a time. That stretch belongs to the serial of
  the `Locked` that opened it, and events for another serial are ignored until it ends. The
  dialog pins that serial (not `view.live()`). `Request::ask(serial)` opens nothing unless
  `serial` is the stretch's.
- `daemon/mod.rs`, still with one live session:
  - at ready, `hub.session_up(&serial, session.tx.clone(), session.helper_sha())`;
  - at close, `hub.session_down(&serial)`;
  - `hub.set_paused(paused)` replaces `set_session`;
  - each `HelperWedged{serial}` ends that serial's session;
  - the PIN path calls `hub.send_unlock(&serial, pin)`: `Ok(false)` with no session for that
    serial keeps today's "isn't connected" notice, and with a session but no stretch it logs
    `unlock: <serial>'s lock stretch is over; PIN dropped`.

- [ ] **Step 1: Failing tests**
  - `clipboard/mod.rs`:
    - `fan_pc_copy_goes_to_every_phone`: add A, B; offer `x` gives `[(A,x),(B,x)]`.
    - `fan_phone_copy_goes_to_the_others_not_its_source`: `from_phone(A,"x") == "x"`; offer `x`
      gives `[(B,"x")]`.
    - `burst_never_echoes_to_source`: `from_phone(A,"x")`, `from_phone(B,"y")`; offer `x` gives
      `[]`; offer `y` gives `[(A,"y")]`.
    - `fan_secret_reaches_nobody_and_clears_memories`.
    - `fan_removed_phone_gets_nothing`.
    - `fan_reset_forgets_echoes`.
    - `in_flight_is_capped`: after 9 `from_phone` calls, an offer equal to the first sends to
      every phone, because it is no longer in flight.
  - `ipc_phones_tests.rs` (Hub with two links over `mpsc` channels; clients from
    `UnixStream::pair()` through a test-only `add_client`):
    - `same_app_on_two_phones_is_two_windows`: opens of `com.x` with `phone` A and B get ids 1
      and 2. A's helper sees only `open` on 1, B's only on 2. A second open for A focuses
      window 1.
    - `helper_frames_only_reach_their_own_phones_windows`: B sends `video` on stream 1 (A's
      window). A's client gets nothing, and A's own frame on 1 arrives.
    - `client_frames_go_to_their_phones_helper`: input on window 2 reaches only B's helper.
    - `session_down_closes_only_that_phones_windows`: after `session_down(A)`, A's client reads
      EOF and B's window still routes. A new open for A after `session_up(A)` gets id 3 (never
      reused).
    - `open_without_phone_goes_to_the_default_phone`: B came up last, so it goes to B.
    - `lock_state_is_per_phone`: `locked{clipboard}` from B makes `locked(B)` true and `locked(A)`
      false, with one `LockEvent::Locked{serial:B,..}`. A blocked open on B is rewritten to
      `PHONE_LOCKED`; one on A is not.
    - `pc_copy_fans_out_and_phone_copy_skips_its_source`: the test `ClipHandle` gets A's `clip`;
      the echo offer reaches only B.
    - `wedged_helper_is_reported_alone`: A's queue is full and never drained; `on_pc_clip`
      returns `[HelperWedged{A}]` within 1.5 s, and B got its clip.
    - `send_unlock_only_while_that_phone_is_locked`: B unlocked returns `Ok(false)` and its helper
      sees nothing; B locked returns `Ok(true)` with one `unlock` frame to B only.
    - `status_top_level_is_the_latest_ready_phone`: links A then B; status `serial == B`, and
      windows carry `phone`.
  - `tests/ipc.rs`: the existing single-phone tests pass unchanged, except that expected
    `WindowInfo` values gain `phone: Some("dev")`.
  - `tests/unlock.rs`: events with serials. New `events_for_another_phone_wait_for_the_stretch`:
    a stretch for `a`, then `Locked{b}`, shows no second notice in this task, and the answer for
    `a` carries `a`.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Log lengths, never clipboard text. `docs/protocol.md`'s window-IPC
  section is updated in Task 8.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS; goldens unchanged; fingerprint
  identical.
- [ ] **Step 5: Device check:** none of its own (still one session). Pass A (Task 8) re-runs every
  single-phone item on it.
- [ ] **Step 6:** Commit with `feat(ipc): per-phone links, window ids unique across phones, open.phone routing, clipboard fan-out without echoes`.

### Task 5: The run loop serves several phones at once

**Files:**
- Create: `host/lantern/src/daemon/runs.rs`
- Modify: `daemon/mod.rs`, `daemon/phones.rs`, `daemon/control.rs`, `daemon/session.rs`
  (`poll_next`)
- Test: `runs.rs` units, `phones.rs` units, `tests/ipc.rs`, `tests/fake_helper.rs` (adapted)
- Modify: `tools/device_test.py` (items 28–31)

**Interfaces (Produces):**
- `SessionConn::poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Inbound>` over
  `rx.poll_recv`/`done.poll_recv`, with today's `next()` semantics. `next()` becomes
  `poll_fn(|cx| self.poll_next(cx)).await`.
- `runs.rs`:
  ```rust
  pub struct PhoneRun<F> {
      pub active: Option<(SessionConn, Instant)>,
      pub starting: Option<Pin<Box<F>>>,
      pub retry_at: Option<Instant>,
      pub failures: u32,
      /// Offline while wanted: the end of its unplug grace.
      pub grace: Option<Instant>,
  }
  pub struct Runs<F> { /* BTreeMap<String, PhoneRun<F>>, round-robin cursor */ }
  impl<F: Future<Output = Result<SessionConn>>> Runs<F> {
      pub fn new() -> Self;
      pub fn want(&mut self, serial: &str) -> &mut PhoneRun<F>;   // new: retry_at = now
      pub fn is_wanted(&self, serial: &str) -> bool;
      pub fn get_mut(&mut self, serial: &str) -> Option<&mut PhoneRun<F>>;
      pub fn release(&mut self, serial: &str) -> Option<PhoneRun<F>>;
      pub fn serials(&self) -> Vec<String>;
      pub fn state(&self, serial: &str) -> Option<PhoneState>;  // Ready|Starting|Reconnecting|Retrying, as 4b's state_of
      pub fn next_wake(&self) -> Option<Instant>;               // min retry_at (no active/starting) and grace ends
      pub async fn next_started(&mut self) -> (String, Result<SessionConn>); // cancel-safe; pending if none
      pub async fn next_inbound(&mut self) -> (String, Inbound);             // cancel-safe; pending if none
  }
  ```
  Both `next_*` are `poll_fn`s over the map, starting at a rotating cursor, so no phone starves.
  The boxed futures stay in the map when the call is dropped.
- `phones::on_plug(serial, configured, autoconnect, off, live, pending) -> OnPlug`, in this
  order:
  1. `configured`: Connect for that serial, Ignore for any other.
  2. `pending`: Connect.
  3. `live`: Connect.
  4. `autoconnect && !off`: Connect.
  5. `autoconnect && off`: Wait.
  6. `!autoconnect`: Prompt.
- `phones::Timing` gains `list_wait: Duration` (default 30 s; decision 14).
- `control.rs`: `View::live_phones(&self) -> impl Iterator<Item = &PhoneView>`. `View::live()`
  stays until Task 6 removes it.
- `drive` semantics. The phone table stays; `wanted`, `grace`, `active`, `starting`, `retry_at`
  and `failures` move into `Runs`.
  - **Online(S):** the 4b blip and grace rules apply per phone.
    - S wanted and in its grace: the grace ends and a start is due now.
    - Otherwise `on_plug` decides, and Connect does `runs.want(S)`. No other phone is released,
      and no other phone's `live`, `off` or pending request changes.
  - **Offline(S):** if S is wanted, only S's run cancels its start, closes its session
    (`hub.session_down(S)`) and enters the grace. Otherwise S enters `leaving` as today.
  - **Grace end (S):** `runs.release(S)`, `set_live(S, false)`, and `Gone{S}`.
  - **Due start (S):** S is plugged and not being undone; the start goes into S's run.
  - **Ready(S):** `hub.session_up`, then `set_live(S, true)`, `Gone{S}` and the pending open, as
    today.
  - **Start failure / helper death (S):** backoff on S's run only. A `HandshakeError` still ends
    lanternd.
  - **Inbound (S, frame):** `hub.on_helper(&S, frame)`. Every `HelperWedged{serial}` ends that
    serial's session alone, with backoff.
  - **Connect(S):** `set_off(S, false)`; pending as today, minus `keep_pending_for` (decision 5).
    If S is plugged and not wanted, `runs.want(S)`. If S is wanted and backing off, retry now.
    If S is being undone, the command is refused with the notice `Lantern is still being
    removed from {name}` / `Connect it again when that's done.`
  - **Disconnect(S):** flags first, as today, then release S's run only.
  - **UndoSetup(S):**
    - If another undo runs, it is refused with the notice `Lantern is busy removing itself from
      {other}` / `Try {name} again when that's done.`
    - Otherwise flags first, release S's run only, then the undo.
    - Commands for any other phone run normally meanwhile (decision 6).
  - **Quit:** `set_live(false)` for every wanted phone, then every run is closed.
  - **Stop signal:** every run is closed and no flag changes.
  - **Start-up rule:** stale `live` flags are cleared when
    `(listed && now >= start + unplug_grace) || now >= start + unplug_grace + list_wait`.
  - **View:** `PhoneView.state` comes from `runs.state(S)`, else Undoing, Off, Waiting or
    Unplugged as today. `locked = hub.locked(S)`, `companion = hub.companion(S)`.
    `hub.set_paused` is set only when no phone is live.
  - **Names** from the device records are unchanged in the notices; bodies are escaped.
- `daemon::run` is unchanged apart from passing `Timing` as today.

- [ ] **Step 1: Failing tests**
  - `runs.rs`:
    - `inbound_from_every_session`: two `SessionConn::attach` over local TCP pairs; frames written
      by each peer arrive tagged with their serial, both within 1 s while the other side is busy.
    - `next_started_reports_its_phone`: a ready future for `b` and a pending one for `a` give
      `("b", Ok)`, and `a`'s is still in its run.
    - `release_leaves_the_other`.
    - `state_of_each_phone`.
    - `next_wake_is_the_earliest`.
  - `phones.rs`: `on_plug_table` is rewritten for the six rules (one assert per rule; `Ignore`
    for another serial under `configured`).
  - `tests/ipc.rs`. A new `start_pair(ports: &[(&str, u16)], opts)` starter maps each serial to its
    own `FakeHelper`. Serials are `dev` (Pixel) and `b`.
    - `two_phones_live_at_once`: autoconnect on, both online: both ready, each helper had one
      session.
    - `connecting_b_keeps_a_live` (replaces `checking_b_switches_a_off_without_off_flag`):
      autoconnect off; Connect dev, then b: both ready. `dev`'s helper is quiet (no `bye`) and its
      record still has `live:true`.
    - `windows_of_two_phones_route_to_their_own_helper`: one window per phone with distinct ids;
      status `windows` carry `phone`.
    - `helper_death_of_one_phone_keeps_the_other`: b's helper dies; b gets a new session after
      backoff. dev's window client stays open, dev's helper is quiet, and the view keeps dev
      `Ready` throughout.
    - `offline_of_one_phone_keeps_the_other`: Offline(b) makes b `Reconnecting`; dev's window
      routes a frame both ways meanwhile.
    - `disconnect_one_keeps_the_other`.
    - `undo_of_one_keeps_the_others_session_and_windows`: the fake runner is called with `b`;
      dev's window is open after it.
    - `connect_during_its_undo_says_so`: the notes hold `Lantern is still being removed from b`;
      no start for b.
    - `commands_for_other_phones_run_during_an_undo`: Disconnect dev during b's undo gives dev
      `Off`.
    - `second_undo_is_refused_with_a_notice`.
    - `connect_for_b_keeps_a_pending_request`: a pending (a, com.x), then Connect b, then a
      online: opener called with (a, com.x, 0).
    - `quit_clears_live_of_every_phone`: both records `live:false`; both helpers read EOF.
    - `stop_ends_every_session_and_keeps_flags`: both helpers read EOF; both records keep
      `live:true`.
    - `phone_copy_reaches_pc_and_other_phone_only`: with a fake `PcClipboard`, dev's `clip("x")`
      sets the PC. The offer `x` sends `clip x` to b only. A PC offer `y` reaches both.
    - `pin_for_an_unlocked_phone_is_dropped`: b locked, a PIN for dev: no `unlock` frame anywhere.
      A PIN for b reaches b's helper only.
    - `stale_live_cleared_even_if_adb_never_lists`: `unlisted`, grace 300 ms, `list_wait` 300 ms:
      the record's `live:false` within 1 s.
    - `status_top_level_is_the_latest_ready_phone`.
    - The 4b tests whose rule changed: `live_phone_gone_starts_nothing` and
      `per_phone_off_flags_are_independent` stay valid and must pass. `pending_is_per_phone`
      stays. `autoconnect_on_connects_unless_off` is unchanged.
  - `tests/fake_helper.rs`: the restart and backoff tests pass against `run_loop` unchanged.
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Move `release!`/`close_session`/`cancel_start` into `Runs`/`PhoneRun`
  helpers (`close_run(run, hub)` closes the session and calls `hub.session_down`). The loop's
  doc comment states the new rules.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS; fingerprint identical.
- [ ] **Step 5: Device items** (DEFERRED to Task 8). All are in `PAIR` and run inside
  `two_phones()`. `S = SERIAL`, `O = OTHER`.
  - **Item 28 `two_phones_live`** (automatic):
    1. `Window(SETTINGS, …, phone=S)` and `Window(SETTINGS, …, phone=O)` are both `opened` within
       20 s.
    2. `status()["windows"]` holds two `com.android.settings` entries with distinct ids and
       `phone` S and O.
    3. KWin has windows with `resourceClass` `app_id_of(SETTINGS, S)` and `app_id_of(SETTINGS,
       O)`, which differ.
    4. Each phone's `lantern_displays()` has one `lantern-` display.
    5. `adb forward --list` has one `lantern` forward per serial.
    6. A second `lantern open --phone S com.android.settings` exits 0 within 5 s, and S's display
       count is unchanged.
    7. Close both through KWin.
  - **Item 29 `clipboard_fans_out`** (automatic once both are unlocked). Texts with distinct
    lengths per step.
    1. A Klipper set reaches both: each phone's `logcat_since` has `clip: PC clipboard (N chars)
       set on the phone`.
    2. S's test screen copies (item 13's debug steps on a `Window(COMPANION, phone=S)`). Klipper
       then holds it; O logs `… (M chars) set on the phone` within 3 s; S logs no `(M chars) set
       on the phone` for 5 s.
    3. Repeat with the phones swapped.
    4. Restore the PC clipboard first in cleanup; disable both test screens.
  - **Item 30 `one_phone_failure_isolated`** (automatic):
    1. Note O's helper pids and keep an O Settings window open.
    2. Kill S's helper: `adb -s S shell "kill -9 \$(pgrep -f '^app_process / dev[.]lantern[.]helper[.]Main')"`,
       one quoted string.
    3. S is ready again within 20 s with new helper pids.
    4. O's pids are unchanged, its window process is running, `status()` still lists O's window,
       and the journal since the kill has no `session closed` for O.
    5. `lantern disconnect S`: O is unchanged the same way. Then `lantern connect S`.
  - **Item 31 `unplug_one_keeps_other`** (`--pair --manual`). `STEPS`: "1. Pull the Xiaomi's
    cable and keep it out for at least 15 s. 2. Plug it back in."
    1. Keep an O Settings window open.
    2. Wait for S off adb.
    3. For 15 s: O ready with the same helper pids and its window alive.
    4. Wait for S back on adb, then `lantern connect S` (autoconnect is off in the harness), ready
       within 20 s.
- [ ] **Step 6:** Commit with `feat(daemon): every checked phone has its own live session; one phone's trouble never touches another's`.

### Task 6: Unlock per phone; the tray for several live phones

**Files:**
- Modify: `host/lantern/src/unlock.rs`, `tray/menu.rs`, `tray/mod.rs`, `daemon/control.rs`
  (remove `View::live()`)
- Test: `tests/unlock.rs`, `menu.rs` units, `tests/tray.rs`
- Modify: `tools/device_test.py` (item 32)

**Interfaces (Produces):**
- `unlock::run` keeps per-phone state:
  ```rust
  struct PhoneLock { stretch: bool, ended: u64, shown: u32 }
  // run(): locks: HashMap<String, PhoneLock>, asking: Option<Asking>, queued: VecDeque<String>
  fn begin_asking(locks: &HashMap<String, PhoneLock>, view: &watch::Receiver<View>,
                  prompt: &Prompt, serial: &str, from: &str) -> Option<Asking>;
  ```
  - **Events:**
    - `Locked{serial}` opens that phone's stretch and shows its own notice. Another phone's notice
      is never replaced.
    - `Result{serial, ok:false}` replaces that phone's notice.
    - `Unlocked`, or `Result{ok:true}`, ends that phone's stretch (`ended += 1`) and closes its
      notice.
  - **Opening a dialog:**
    - A notification action finds the serial whose `shown == id`.
    - A `Request(serial)` needs that phone's stretch.
    - If a dialog is open, the serial is queued (at most once).
    - When a dialog's answer is handled, the next queued serial still in a stretch gets its
      dialog (`from = "queued"`).
  - **Answer:** goes to `pins` as `(serial, pin)` only if `asked_in == locks[serial].ended`;
    otherwise it is wiped with the log line `the lock stretch ended during the PIN dialog`.
  - **Names:** `phone_name(view, serial)` is `view.phone(serial)`'s display name, else
    `"phone"`.
- `tray/menu.rs`:
  - Row 15 is gone.
  - `status_line` and `overlay` follow decision 19 and these rules: busy (`view-refresh`) if any
    live phone isn't Ready or any phone is Undoing; `emblem-warning` if any Ready phone has a
    companion msg; `media-playback-pause` if none is live and one is listed; else `""`.
  - Rows `100n`/`100n+1`/`100n+2` are unchanged, per phone.
- `control.rs`: `View::live()` is removed; callers use `live_phones()` or `phone(serial)`.

- [ ] **Step 1: Failing tests**
  - `tests/unlock.rs` (fake notification server, fake dialog program as today):
    - `two_phones_two_notices`: `Locked{a}`, `Locked{b}` give two `Notify` calls with
      `replaces_id` 0, titled with each name.
    - `answer_goes_to_the_phone_asked_for`: a dialog for a is open; `Locked{b}` arrives; the
      dialog answers. `pins` gets `("a", _)`, never b.
    - `click_for_b_while_a_asks_is_queued`: b's action during a's dialog opens b's dialog after
      a's answer (the dialog program ran twice, the second time with b's name). If b unlocked
      before that, it is not opened.
    - `unlock_of_one_closes_only_its_notice`.
    - `answer_after_its_stretch_ended_is_wiped`: `Unlocked{a}` during a's dialog: no PIN sent.
    - `tray_request_needs_that_phones_stretch`.
    - The existing single-phone tests pass with serials.
  - `menu.rs`:
    - `two_live_phones_tooltip`: `Connected to Pixel and b`.
    - `three_live_phones_tooltip`.
    - `connecting_phone_shows_first`.
    - `no_one_at_a_time_row` (id 15 never present).
    - `locked_rows_per_phone`: b locked gives rows 201 `Locked` and 202 `Unlock b…`; a has none.
  - `tests/tray.rs`: `unlock_row_click_names_its_phone` (the Request carries that serial).
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS.
- [ ] **Step 5: Device item 32 `lock_one_keeps_other`** (DEFERRED to Task 8; `PAIR` and `MANUAL`;
  O is the Motorola, checked as `O == "ZD222LMDFV"`). `STEPS`: "1. When 'Unlock your edge 50 pro'
  shows, click Enter PIN… and type the PIN."
  1. Keep an S Settings window open.
  2. Lock O with `adb -s O shell input keyevent KEYCODE_SLEEP`, then wait for `deviceLocked=1`
     (15 s).
  3. Set a Klipper text (length N).
  4. Within 10 s: a Notify `Unlock your edge 50 pro`, the journal has `phone locked` for O, and S
     logs `(N chars) set on the phone`. S's helper pids are unchanged and its window is alive; the
     tray has rows `Locked`/`Unlock edge 50 pro…` only under O's slot (`tray_rows`).
  5. Wait `MANUAL_WAIT` for the journal's `unlock result ok=true` and O's `held PC copy (N chars)
     set on the phone` in logcat.
  6. Restore the PC clipboard.
- [ ] **Step 6:** Commit with `feat(unlock): one lock stretch and notice per phone, one PIN dialog at a time, the tray for several live phones`.

### Task 7: Notices: a replug keeps the offered app, notices close on exit, escaped bodies

**Files:**
- Modify: `host/lantern/src/connect.rs`, `unlock.rs`, `daemon/mod.rs` (`run` waits for the
  prompts), `daemon/control.rs` (`undo_notice`), `daemon/catalog.rs` (window notice bodies),
  `tray/mod.rs` (`undo_question`)
- Test: `tests/connect.rs`, `tests/unlock.rs`, `control.rs`/`catalog.rs`/`tray/mod.rs` units
- Modify: `tools/device_test.py` (items 33, 34)

**Interfaces (Produces):**
- `connect.rs`:
  - `Shown` gains `app: Option<String>` (the offered app's name).
  - A `PluggedIn{serial}` whose shown notice carries `connect: Some(Some(open))` keeps `open` and
    `app`, and shows `plugged_with_app_notice(name, app)`: summary `{name} is plugged in`, body
    `Connect it to open {app} here.` (escaped). Its Connect sends `Command::Connect{serial, open:
    Some(open)}`.
  - When `events` closes, `run` closes every shown notice before it returns.
- `unlock::run` closes every shown notice when `events` closes.
- `daemon::run`:
  - keeps the `JoinHandle`s from `connect::spawn` and `unlock::spawn`;
  - after `serve_with` returns (Quit or a signal; every sender is dropped with the hub and the
    controls), awaits both within `SHUTDOWN_NOTICES = 3 s`;
  - then returns its result.
- Escaping (`notices::escape_markup`, bodies only):
  - `control::undo_notice` body;
  - `catalog::connect_notice`, `connect_notice_for_phone` and `paused_notice` bodies (app and phone
    names).
- kdialog:
  - `tray::undo_question(name)` returns `<qt>Remove Lantern from {esc}? … removes {esc}'s apps
    from its menu folder. …</qt>`;
  - `unlock::Prompt::kdialog()` args end with `"<qt>Enter the PIN for {name}</qt>"`;
  - `ask()` substitutes `escape_markup(name)` for `{name}`.

- [ ] **Step 1: Failing tests**
  - `tests/connect.rs`:
    - `plugged_in_keeps_the_offered_app`: `OpenRefused{dev, app "Files", com.x, 0}`, then
      `PluggedIn{dev}`. The second `Notify` replaces the first id, its body is `Connect it to open
      Files here.`, and the action gives `Connect{dev, open:Some(("com.x",0))}`.
    - `plugged_in_without_an_offer_is_unchanged`.
    - `notices_close_when_events_end`: two shown notices, then the events sender is dropped: two
      `CloseNotification` calls with their ids before `run` returns.
  - `tests/unlock.rs` `notice_closes_when_events_end`.
  - `control.rs` `undo_notice_body_is_escaped`: output `x <y> & z` gives `x &lt;y&gt; &amp; z`;
    the summary keeps the raw name.
  - `catalog.rs` `window_notice_bodies_are_escaped`: app `A&B`, phone name `<P>`.
  - `tray/mod.rs` `undo_question_is_escaped_rich_text`: name `A&B` gives text starting `<qt>Remove
    Lantern from A&amp;B?` and ending `</qt>`.
  - `unlock.rs` `kdialog_prompt_is_escaped_rich_text`: the `{name}` arg for `A<B` is `<qt>Enter
    the PIN for A&lt;B</qt>` (the test calls `ask` with a fake program that prints its args to
    a scratch file, never the real kdialog).
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests and clippy. Expected: PASS; fingerprint identical.
- [ ] **Step 5: Device items** (DEFERRED to Task 8):
  - **Item 34 `notices_close_on_quit`** (automatic, single phone, `NotifyMonitor`):
    1. `lantern disconnect SERIAL`.
    2. `lantern open --phone SERIAL com.android.settings` exits 1, and a `Connect your …` Notify
       shows.
    3. `systemctl restart` lanternd.
    4. Within 5 s the monitor log has a `CloseNotification` with that Notify's id. Read the
       returned id from the monitor's `method return` for that call.
    5. Cleanup: `lantern connect SERIAL`, then `wait_ready`.
  - **Item 33 `replug_keeps_requested_app`** (`MANUAL`; it lifts `LANTERN_SERIAL` with
    `manager_env(unset=["LANTERN_SERIAL"], set={"LANTERN_UNPLUG_GRACE_MS":"0"})`, autoconnect
    off, `only_this_phone(33)`). `STEPS`: "1. Pull the Xiaomi's cable. 2. When 'Connect your
    Xiaomi 11T Pro' shows, plug the phone in. 3. On 'Xiaomi 11T Pro is plugged in', click
    Connect."
    1. Wait for the phone off adb; `lantern open --phone SERIAL com.android.settings` exits 1, and
       a `Connect your` Notify carries `connect`.
    2. Wait for the phone back on adb: a Notify `<name> is plugged in` whose body names Settings.
    3. Wait `MANUAL_WAIT` for the journal's `Connect clicked … open=Some(("com.android.settings",
       0))`.
    4. Ready, then one Settings window (KWin) within 20 s, and `status()["windows"]` holds exactly
       one `com.android.settings` 10 s later. Close it.
- [ ] **Step 6:** Commit with `fix: a replug keeps the offered app; notices close when lanternd stops; notice bodies and kdialog texts escaped`.

### Task 8: Docs and device acceptance

**Files:**
- Modify: `docs/protocol.md`, `README.md`, `docs/HANDOFF.md`, `tools/device_test.py` (docstring
  item list)

- [ ] **Step 1: Docs.**
  - `protocol.md`:
    - "Connect rules" lose "one phone at a time" and the wanted-phone wording; the six rules
      apply, and Undo runs one at a time while other phones carry on.
    - Window IPC: ids unique across phones and never reused within a run; `open` routing by
      `phone`, else the default phone.
    - `status_report`: the default-phone top level and `windows[].phone`.
    - Clipboard: the fan-out rule, the in-flight list and the stale echo.
    - Lock rules: `"session"`, one stretch per phone.
  - README: several phones live, G9.
  - HANDOFF:
    - status of Plan 4c;
    - decisions 1–20;
    - Plan 4b decisions 4 and 11 marked superseded;
    - Known minor issues: remove the fixed ones (first-list cap, menu.json rerun, notices on quit,
      Connect during Undo, escaped bodies, PIN after the stretch, the free-port race, item 8's
      whole-buffer logcat) and add the reviews' deferred minors;
    - Next.
- [ ] **Step 2: Before installing**, back up the real launcher files, menu fragments, Lantern
  state and settings to scratch (`references/device-acceptance.md` §1). Ask Sahaj before
  installing if `lantern status` shows windows he is using: `build.py install` restarts lanternd.
- [ ] **Step 3: Give Sahaj every step in one message before starting**, then run the passes and
  verify each from the lanternd journal (`journalctl --user -u lanternd --since … | grep -vE
  'DEBUG|clipboard::|catalogue'`) and each phone's logcat, never from a chat answer. Run each
  manual command in the background with a PTY through `tee` to a scratch log. The suite prints
  its own steps and waits up to 3600 s on observed state. The message to Sahaj:
  1. Keep the desktop unlocked and both phones plugged in. I install the new build; lanternd
     restarts.
  2. Pass A (automatic, ~15 min, nothing to do): the suite pins the Xiaomi.
  3. Pass B (two phones): when "Unlock your edge 50 pro" appears, click Enter PIN… and type the
     PIN. Nothing else to do.
  4. Manual, one at a time, at your pace. I'll say which one is running:
     - item 8: pull the Xiaomi's cable, keep it out at least 15 s, plug it back;
     - item 14: the same again;
     - item 31: pull the Xiaomi's cable at least 15 s, plug it back;
     - item 32: when "Unlock your edge 50 pro" shows, unlock it with the PIN;
     - item 27 (Motorola alone served): when "Unlock your edge 50 pro" shows right after connect,
       type the PIN, then leave the Motorola alone for 5 minutes;
     - item 21 (Motorola): the right PIN, then the deliberately wrong PIN, then the right PIN.
       The wrong PIN counts toward the phone's lockout;
     - unplug the Motorola for items 24, 25, 26 and 33 (Xiaomi only). 24: pull and replug the
       Xiaomi twice. 25: pull and replug once. 26: click Connect on "Connect your Xiaomi 11T
       Pro". 33: pull the Xiaomi, plug it in when "Connect your Xiaomi 11T Pro" shows, then
       click Connect on "Xiaomi 11T Pro is plugged in".
  5. At the end I tell you which phone is connected and that every setting is back.
- [ ] **Step 4: Pass A** (both phones attached, the preflight pins `LANTERN_SERIAL=20e9318`):
  `python3 tools/build.py install && python3 tools/device_test.py`.
  - Expected: every automatic item PASS: 1–7, 9–13, 15–20, 22, 23 and 34. Item 22 now includes
    "no prompt after a restart" (G9 on a swipe phone).
  - Record the summary line in HANDOFF.
- [ ] **Step 5: Pass B** (both phones attached):
  `python3 tools/device_test.py --pair --manual --only 28 29 30` (`--manual`: the Motorola may lock between items and G9 asks for its PIN).
  - Expected: 3/3 PASS. The harness unlocked the Motorola through the G9 notice when it was
    locked.
  - Afterwards: both phones `restored`, both records' `off`/`live` as before, `LANTERN_SERIAL`
    back.
- [ ] **Step 6: Manual items**, each its own run, re-checking `adb devices` and the baseline
  between items. A failed cable item stops progression until the phone is back.
  1. `--manual --only 8`, then `--manual --only 14` (Xiaomi). Item 8's panel restore is read from
     logcat (`cleanup: panel on` since the yank) and SurfaceFlinger power mode 2.
  2. `--pair --manual --only 31 32`.
  3. `LANTERN_SERIAL=ZD222LMDFV python3 tools/device_test.py --manual --only 27 21` (Motorola; `--only` runs in item order, so 21 runs first).
  4. With the Motorola unplugged: `--manual --only 24 25 26 33`.
- [ ] **Step 7: Hand checks.**
  - The tray with both phones live shows two checked rows and no one-at-a-time row. Take a
    screenshot.
  - The Remove confirm dialog shows "edge 50 pro" with no `<qt>` visible (kdialog rich text;
    cancel it).
  - With both live, open one app from each phone's Kickoff folder: two taskbar groups.
  - Tell Sahaj which phone is left connected and that both phones' stay-awake and keyboard are
    their originals (`phone_state` per serial).
- [ ] **Step 8:** Commit with `docs: Plan 4c acceptance, protocol and README updates`.
