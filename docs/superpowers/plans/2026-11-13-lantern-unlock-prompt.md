# Lantern Unlock Prompt (Plan 4a) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** If a PIN-locked phone blocks a copy, a paste or an app opening, Lantern shows one
notification: "Unlock your <phone>". It has an "Enter PIN…" button that opens a KDE password
dialog, and Lantern types that PIN into the phone's lock screen. Lantern then checks that the
phone really unlocked and finishes what was blocked.

**Architecture:**
- **Helper (Java).** Detects the lock with `LockState` (parses `dumpsys trust`).
  - It reports to lanternd with `locked{reason}` and `unlocked`.
  - It holds back a PC copy that arrived while the phone was locked, and writes that copy after
    the unlock.
  - It runs the unlock: `Unlocker` sends every event to display 0, checks which window has focus
    on display 0 by parsing `dumpsys window displays`, and types the PIN as key events. It
    answers with `unlock_result{ok}`.
- **lanternd (Rust).**
  - Keeps the lock state in `View.locked`.
  - Shows the notification with an action over `org.freedesktop.Notifications`, using zbus.
  - Asks for the PIN with `kdialog --password` and sends it once as `unlock{pin}`. The PIN is a
    `Pin` value that is never logged and is overwritten when it is dropped.
  - After an unlock, it reopens any app that couldn't open.
- **Tray.** Shows an "Unlock <phone>…" row while the phone is locked. The row starts the same
  prompt.

**Tech Stack:** As in Plan 3: Rust with zbus 5 (already a dependency), Java helper. No new crate.

**Spec:** `docs/superpowers/specs/2026-11-13-lantern-unlock-prompt-design.md` (approved
2026-11-13, spike result included). It builds on `docs/superpowers/specs/2026-11-07-lantern-design.md`
§4 (anti-goals) and §9 risk 2 (locked phone). Read `docs/HANDOFF.md` ("Field findings on the
Motorola", "Plan 3 decisions") and `docs/protocol.md` first.

**Out of scope:** patterns and letter passwords, mirroring the lock screen into a window, a work
profile with its own separate lock, prompting when nothing is blocked.

## Global Constraints

- Phones:
  - Motorola edge 50 pro `ZD222LMDFV`: PIN keyguard, the test phone for this plan.
  - Xiaomi `20e9318`: swipe keyguard, `deviceLocked=0`. It must never prompt.
- **The PIN is never stored, logged or put in a process argument.** That covers disk, settings,
  the journal, `tracing`, `Droid.log`/logcat, exception messages, `Debug` output, `adb shell`
  argv and `ps`.
  - It lives in memory for one attempt: lanternd's `Pin`, the `unlock` frame and the helper's
    `char[]`.
  - lanternd's `Pin` is zeroed on drop. The helper's `char[]` is zeroed after typing.
  - The `unlock` frame's payload is never logged on either side, including parse errors.
- **One unlock attempt per submitted PIN.** No automatic retries.
- **Before typing,** the helper checks two things: the phone is still locked
  (`LockState.locked()` is true), and display 0's focused window is the keyguard. If either check
  fails, it types nothing and answers `ok:false` with a reason.
- **Every event Lantern injects for the unlock targets display 0.** An untargeted event lands on
  the focused Lantern window (spike, design doc).
- **The panel goes back off after an unlock attempt** if the session had it off, whether the
  attempt succeeded or not.
- **Protocol stays v1.** This plan only adds new message types, and existing bytes and goldens
  are unchanged. The jar and the APK ship inside `lantern`.
- **One "Unlock your <phone>" notification per locked stretch.** A stretch ends when the helper
  sends `unlocked`.
- The PIN must be 4–16 ASCII digits. Anything else is refused on the PC, and nothing is sent.

## Review Focus

1. **The PIN leaks.** It must not reach a log line, an error string, `Debug`, an adb argument, or
   a frame logged on a protocol error.
   - Pinned by Task 1 `unlockPayloadNeverLogged`, Task 2 `unlockNeverLogsPin`, and Task 3
     `pin_debug_is_redacted` / `unlock_frame_not_logged`.
2. **Keys go to the wrong place.** Two cases: Windows App is focused, or the phone was unlocked
   by hand between the prompt and the submit.
   - Expected: nothing is typed, and the result is `ok:false` with reason `lock screen not
     reached` or `already unlocked`.
   - Pinned by Task 2 `typesNothingUnlessKeyguardFocused` and `alreadyUnlockedTypesNothing`.
3. **A wrong PIN.** Expected: exactly one attempt and one failure notification, and the panel
   is off again.
   - Pinned by Task 2 `wrongPinOneAttemptPanelBackOff`, and Task 3
     `wrong_pin_notifies_once_no_retry`.
4. **A notification storm.** Twenty PC copies while the phone stays locked must produce one
   notification. Pinned by Task 3 `one_notice_per_locked_stretch`.
5. **No tray host, or no notification server.** lanternd runs as before. The tray row still
   offers the unlock. Pinned by Task 3 `no_notification_server_is_harmless`.

---

## File Structure

```
android/helper/src/dev/lantern/helper/
  LockState.java    NEW  locked() from `dumpsys trust` (pure parser + device impl)
  Unlocker.java     NEW  the unlock sequence over Power/Injector/FocusProbe/LockState fakes
  FocusProbe.java   NEW  display 0's focused window from `dumpsys window displays` (pure parser)
  Router.java, Main.java, Msg.java, ClipSync.java, PhoneClipboard.java     MODIFY
host/proto/src/lib.rs, host/proto/tests/golden.rs, host/proto/golden/*.bin MODIFY (new types)
host/lantern/src/daemon/{ipc,mod,control}.rs                               MODIFY
host/lantern/src/unlock.rs   NEW  Pin, prompt flow, notification with actions (zbus)
host/lantern/src/tray/{menu,mod}.rs                                         MODIFY (row 13)
host/lantern/src/window/{outcome,mod}.rs                                    MODIFY (Notice::Locked)
tools/device_test.py (items 21-22), docs/protocol.md, README.md, docs/HANDOFF.md  MODIFY
```

Test commands:
- Host: `cd host && export PATH="$HOME/.cargo/bin:$PATH" && cargo test --workspace --offline`
- Helper: `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py helper`
- Install: `python3 tools/build.py install`. This restarts lanternd from this build.

---

### Task 1: Lock detection, `locked`/`unlocked`, held PC copy

**Files:**
- Create: `LockState.java`.
- Modify:
  - `Router.java`: the new types, and `unlock` parsing (wired in Task 2).
  - `Main.java`, `Msg.java`, `ClipSync.java`, `PhoneClipboard.java`.
  - `host/proto/src/lib.rs`, `host/proto/tests/golden.rs`, `docs/protocol.md`.
- Test: `LockStateTest`, `ClipSyncTest`, `RouterTest`, `ProtoTest`, proto unit tests.

**Interfaces (Produces):**
- Proto: all new types, on stream 0.
  - `Locked { reason: LockReason }`, helper→daemon. `LockReason` is `Clipboard | Open`
    (snake_case).
  - `Unlocked`, helper→daemon.
  - `Unlock { pin: String }`, daemon→helper. Its `Debug` impl prints `pin: "<redacted>"`: write
    a manual `Debug` for `Control` that delegates to the derived one for every other variant.
  - `UnlockResult { ok: bool, #[opt] reason: Option<String> }`, helper→daemon.
  - New goldens: `control_locked.bin`, `control_unlock_result.bin`. Never add a golden for
    `unlock`.
- Java:
  - `Msg.TYPES` gains `locked`, `unlocked`, `unlock`, `unlock_result`.
  - Router: `Extras` gains `void unlock(char[] pin)`. Router parses `unlock.pin` (only `[0-9]`,
    length 4–16, otherwise `ProtocolError("bad unlock")`, which never includes the payload) and
    hands Extras a `char[]`. It never logs anything about it beyond `router: unlock requested`.
- `final class LockState`:
  - `static Boolean parseTrust(String dumpsys)` reads the `deviceLocked=` value on the line of the
    `(current)` user and returns null if there is none.
  - `interface Source { String dump() throws IOException; }`. The device implementation runs
    `dumpsys trust` with a 3 s timeout.
  - `boolean locked()` returns false when the state is unknown, because Lantern only prompts when
    it is sure.
- ClipSync gains a `LockState` dependency and two methods:
  - `fromHost(text)`: while locked it doesn't write. It keeps the text as `held` (only the
    latest) and calls `onLocked.accept("clipboard")`.
  - `phoneChanged(null, …)`: while locked it also calls `onLocked.accept("clipboard")`.
  - `void unlocked()`: writes `held`, if any, then clears it.
- Main:
  - `onLocked(reason)` sends `locked{reason}`, once per stretch: a `stretch` flag that is cleared
    on unlock.
  - While the stretch is open, a `lantern-lock` thread polls `LockState.locked()` every 2 s. When
    it reads false, Main sends `unlocked`, calls `clipSync.unlocked()`, and stops polling.
  - Router's `Extras` also gains `void openFailed(int win)`, which is called from the
    open-failure path (`router: open win=… failed`) before the `error`/`closed` frames. Main
    implements it as `if (lockState.locked()) onLocked("open")`. The window's `error` keeps the
    helper's message; `locked` is stream 0 only, and the daemon tells the window (Task 3).
- PhoneClipboard.changed(): a null or empty clip calls `sync.phoneChanged(null, false, 0)`, so
  the lock check runs. It used to return without doing anything.

- [ ] **Step 1: Failing tests**
  - `LockStateTest.parsesCurrentUser`: uses the two-user `dumpsys trust` text from the HANDOFF
    field finding. `deviceLocked=1` gives true and `=0` gives false. With no `(current)` line the
    result is null, and `locked()` is false.
  - `ClipSyncTest.lockedHoldsLatestAndWritesOnUnlock`: while locked, two `fromHost` calls write
    nothing and call `onLocked` once per call (Main dedupes the calls). After `unlocked()` only
    the second text is written.
  - `ClipSyncTest.lockedNullPhoneReadReportsLocked`
  - `RouterTest.unlockPayloadNeverLogged`: valid and invalid `unlock` frames. No log line contains
    the digits. An invalid frame gives a `ProtocolError` whose message has no digits.
  - Proto:
    - `locked_json_shape`
    - `unlock_result_json_shape`: `reason` is absent when None.
    - `unlock_debug_is_redacted`
    - `existing_goldens_unchanged` (the golden test as is).
- [ ] **Step 2:** Run the helper build and the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement. Regenerate the goldens. `git diff --stat host/proto/golden` must show
  only the two new files.
- [ ] **Step 4:** Run both test suites. Expected: PASS.
- [ ] **Step 5: Device check** (Motorola, with Sahaj).
  - Use a scratch build of the helper alone, or `build.py install`.
  - While Lantern serves the Motorola, let it lock: `adb -s ZD222LMDFV shell input keyevent
    KEYCODE_SLEEP`, then wait about 5 s. `dumpsys trust` must read `deviceLocked=1`.
  - Copy on the PC. Logcat `lantern` must show `lock: phone locked (clipboard)` once, and no `clip:
    PC clipboard … set`.
  - Do not unlock in this task; Sahaj unlocks by hand. The logcat must then show `lock: phone
    unlocked` within 3 s and `clip: held PC copy … set on the phone`.
- [ ] **Step 6:** Commit with `feat(helper): detect the PIN lock (locked/unlocked), hold a PC copy until unlock`.

### Task 2: The unlock sequence in the helper

**Files:**
- Create: `Unlocker.java`, `FocusProbe.java`.
- Modify: `Main.java` (wires `Extras.unlock`), `Input.java` (key and swipe for a display, if
  needed).
- Test: `UnlockerTest`, `FocusProbeTest`.

**Interfaces (Produces):**
- `final class FocusProbe`:
  - `static String focusOfDisplay(String dumpsysWindowDisplays, int display)` returns the
    `mCurrentFocus=Window{… <title>}` title in that display's section, or null.
  - `static boolean isKeyguard(String title)`: true for a title containing `NotificationShade`,
    `Keyguard` or `Bouncer`.
  - The device `Source` runs `dumpsys window displays` with a 3 s timeout.
- `final class Unlocker`:
  - Constructor: `Unlocker(LockState lock, Power power, Keys keys, FocusProbe.Source focus,
    Sleeper sleep, boolean panelWasOff, Consumer<String> log)`. Its nested types are
    `interface Keys { void key(int display, int keycode); void swipeUp(int display); }` and
    `interface Sleeper { void sleep(long ms); }`.
  - `Result unlock(char[] pin)` returns `final class Result { final boolean ok; final String
    reason; }`, in this order:
    1. If not locked: `ok=true, reason="already unlocked"`, and nothing is typed.
    2. Otherwise:
       1. `keys.key(0, KEYCODE_WAKEUP)`, then sleep 500.
       2. `keys.swipeUp(0)`, then sleep 1000. The swipe goes from (w/2, 0.81h) to (w/2, 0.22h)
          over 200 ms on display 0, where w and h are display 0's real size.
       3. Probe the focus. If it isn't the keyguard: `ok=false, reason="lock screen not
          reached"`.
       4. Otherwise send each digit as `KEYCODE_0+d` down and up, then `KEYCODE_ENTER`, all on
          display 0.
       5. Poll `locked()` every 250 ms for up to 3 s.
    3. The PIN `char[]` is zeroed in a `finally`.
    4. Also in the `finally`: `power.setPanel(false)` if `panelWasOff`.
    5. A wrong PIN gives `ok=false, reason="wrong PIN"`.
  - Log lines (never the PIN): `unlock: start`, `unlock: lock screen not reached (focus=<title>)`,
    `unlock: ok in <ms> ms`, `unlock: still locked`.
- Main: `Extras.unlock(pin)` runs the Unlocker on a `lantern-unlock` thread. One attempt runs at
  a time; a second `unlock` that arrives meanwhile gets `ok=false, reason="busy"`. Main sends
  `unlock_result`. On `ok`, it also ends the stretch, sends `unlocked` and calls
  `clipSync.unlocked()`.

- [ ] **Step 1: Failing tests** (all against fakes)
  - `FocusProbeTest.parsesDisplay0Focus`, with fixture text from the spike: display 0 shows
    `NotificationShade` while a Lantern display shows `com.microsoft.rdc…`. The probe returns
    display 0's title.
  - `UnlockerTest`:
    - `typesDigitsThenEnterOnDisplay0`: every key and the swipe target display 0.
    - `typesNothingUnlessKeyguardFocused`: the focus is `DreamActivity` after the swipe, so no
      digit is sent and the reason is `lock screen not reached`.
    - `alreadyUnlockedTypesNothing`
    - `wrongPinOneAttemptPanelBackOff`: `locked()` stays true. Exactly one ENTER is sent, the
      reason is `wrong PIN`, `setPanel(false)` is called, and the pin array is all zeros.
    - `unlockNeverLogsPin`
- [ ] **Step 2:** Run the helper build. Expected: FAIL.
- [ ] **Step 3:** Implement. The device `Keys` injects through `InputManager` with
  `setDisplayId(0)`, the same way `Input.send` does. The swipe uses `MotionEvent`
  down, moves, then up, with `SOURCE_TOUCHSCREEN`.
- [ ] **Step 4:** Run the helper build. Expected: PASS.
- [ ] **Step 5:** Commit with `feat(helper): unlock the PIN keyguard on display 0 (one attempt, checked)`.
  The device check runs end to end in Task 3, because only lanternd can send `unlock`.

### Task 3: lanternd — notification, PIN prompt, `unlock`, reopen

**Files:**
- Create: `host/lantern/src/unlock.rs`.
- Modify:
  - `daemon/{ipc,mod,control}.rs` and `window/{outcome,mod}.rs`.
  - `notify.rs`, which keeps `send`; actions are new in `unlock.rs`.
- Test: unit tests in `unlock.rs`, `tests/ipc.rs`, `tests/unlock.rs` (private dbus-daemon, as
  `tests/tray.rs` does), device item 21.

**Interfaces (Produces):**
- `unlock::Pin`:
  - `Pin::parse(raw: String) -> Result<Pin, &'static str>` accepts 4–16 ASCII digits, after
    trimming one trailing newline. The error is `"the PIN must be 4 to 16 digits"`.
  - `Debug` prints `Pin(<redacted>)`, and there is no `Display`.
  - `Drop` overwrites the bytes with `std::ptr::write_volatile`.
  - `fn digits(&self) -> &str` is crate-private.
- `View` gains `pub locked: bool`. It is true from `locked` until `unlocked`, a successful
  `unlock_result`, or the end of the session.
- `Parts` gains `unlock: Option<mpsc::Receiver<Pin>>` (capacity 1). While a session is live,
  `drive` turns a received `Pin` into one `Control::Unlock{pin}` frame sent to the helper. With
  no session, the Pin is dropped and the notice is "<phone> isn't connected".
- `Hub` routes `Locked`, `Unlocked` and `UnlockResult` to an `UnlockEvents` channel (an mpsc
  `Sender<LockEvent>` inside `Parts`; `LockEvent` is `Locked(LockReason) | Unlocked |
  Result{ok, reason}`). For a client whose open failed while the view has `locked`, it records
  the pending reopen `(pkg, user)`. There is at most one entry per app, and entries are dropped
  after 2 minutes.
- `unlock::run(conn: zbus::Connection, events, view, pins: mpsc::Sender<Pin>, exe: PathBuf)`:
  - On `Locked`, if the stretch isn't open yet, it calls `Notify("Lantern", 0, "smartphone",
    "Unlock your {name}", body, ["unlock", "Enter PIN…"], {"urgency": 1}, -1)`. The body is
    `"Lantern can't paste or open apps while {name} is locked."`. The notification id is kept.
  - On `ActionInvoked(id, "unlock")`, or the tray's Unlock row: it runs `kdialog --title Lantern
    --password "Enter the PIN of {name}"` through `desktop::spawn` in `spawn_blocking`, reads
    stdout (exit 0), and calls `Pin::parse`. An invalid PIN gives a notification with that
    message, and nothing is sent. A valid PIN goes out with `pins.try_send`.
  - On `Result{ok:false, reason}`: a notification titled `That PIN didn't unlock {name}` for
    `wrong PIN`, otherwise `Couldn't unlock {name}`, with the body set to the reason, and the
    action `["unlock", "Try again"]`.
  - On `Result{ok:true}` or `Unlocked`: it closes the notification (`CloseNotification(id)`) and
    ends the stretch. Then, for each pending reopen, it runs `desktop::spawn(exe, ["open",
    ("--user", N)?, pkg])`.
  - With no notification server (`Notify` errors), it logs one warning. The tray row still works.
- `daemon::run` spawns `unlock::run` on the same session connection approach as the tray (its own
  `Connection::session()` with a 5 s timeout). Without a bus there is no unlock prompt and one
  warning is logged.
- Window: `outcome::Notice::Locked` comes from the error string the helper sends when a lock
  blocked the open. The daemon replaces the helper's open error with `ipc::PHONE_LOCKED: &str =
  "phone locked"` when the view is `locked` and the open failed. The window shows no notification
  of its own (the daemon does) and exits 1 after logging `phone locked; enter its PIN in
  Lantern's notification`.

- [ ] **Step 1: Failing tests**
  - `unlock.rs` units:
    - `pin_parse_accepts_4_to_16_digits`: covers `"1234\n"` (accepted), `"123"`, `"12a4"`, 17
      digits and `" 1234"`.
    - `pin_debug_is_redacted`
    - `pin_drop_zeroes`: through a raw pointer to the buffer, inside a test-only accessor.
  - `tests/ipc.rs`:
    - `unlock_frame_reaches_helper_once`: a Pin is sent and the fake helper sees exactly one
      `unlock` with those digits.
    - `unlock_frame_not_logged`: run with a `tracing` test subscriber capturing output; no line
      contains the digits.
    - `locked_sets_view_and_unlocked_clears`
    - `open_failed_while_locked_gets_phone_locked`
  - `tests/unlock.rs` (private bus as in `tests/tray.rs`, with a fake
    `org.freedesktop.Notifications` that records `Notify`/`CloseNotification` and can emit
    `ActionInvoked`). The PIN dialog command is a field, `unlock::Prompt { program: PathBuf,
    args: Vec<String> }` (`kdialog …` in production), and the tests pass `/bin/echo 1234`:
    - `one_notice_per_locked_stretch`: 20 `Locked` events give one `Notify`.
    - `action_prompts_and_sends_pin`
    - `wrong_pin_notifies_once_no_retry`: `Result{ok:false,"wrong PIN"}` gives one `Notify`
      with "That PIN didn't unlock", and no second Pin is sent.
    - `success_closes_notice_and_reopens_pending`: the reopen command is recorded through a fake
      `exe`, a script that writes its args to a file.
    - `no_notification_server_is_harmless`
- [ ] **Step 2:** Run the host tests. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the host tests. Expected: PASS.
- [ ] **Step 5: Device item 21 `unlock_prompt`** (`--manual`, Motorola, Sahaj types the PIN):
  1. Set `LANTERN_SERIAL=ZD222LMDFV` for the run, as item 20 does for the Xiaomi.
  2. Lock the phone (`input keyevent KEYCODE_SLEEP`, wait until `deviceLocked=1`).
  3. Copy a tag on the PC. Within 3 s the real desktop shows "Unlock your edge 50 pro": check with
     `busctl --user monitor org.freedesktop.Notifications` (a `Notify` call with that summary).
  4. Ask Sahaj to click "Enter PIN…" and type the PIN.
  5. Within 5 s `deviceLocked=0`, the tag is on the phone's clipboard (read back through the
     TestActivity paste path from item 13), and the panel is off again (`dumpsys power` and
     display state as item 7 reads it).
  6. Then a wrong-PIN pass: lock again, Sahaj types a wrong PIN. "That PIN didn't unlock" appears,
     the logcat shows exactly one `unlock: still locked`, and the panel is off. Sahaj unlocks
     with the right PIN through "Try again".
- [ ] **Step 6:** Commit with `feat: unlock prompt (notification, PIN dialog, one checked attempt, reopen)`.

### Task 4: Tray row, never-prompt check, docs

**Files:**
- Modify: `tray/menu.rs`, `tray/mod.rs`, `tools/device_test.py` (item 22), `docs/protocol.md`,
  `README.md`, `docs/HANDOFF.md`.
- Test: `menu.rs` units, `tests/tray.rs`, device item 22.

**Interfaces (Produces):**
- Menu row id 13: Button `Unlock {name}…`, shown right after row 1 while `view.locked`. Its
  action is `Action::Unlock`, which calls the same prompt as the notification action. Give
  `unlock::Prompt` a `request()` handle the tray holds.
- `status_line` for a locked Ready phone is `Connected to {name} (locked)`.

- [ ] **Step 1: Failing tests:** `locked_shows_unlock_row`, `unlock_row_click_prompts` (private
  bus, as in `tests/tray.rs`).
- [ ] **Step 2–4:** Implement until they pass, with the host tests green.
- [ ] **Step 5: Device item 22 `swipe_phone_never_prompts`** (Xiaomi, automatic):
  1. Copy on the PC and on the phone, as item 13 does.
  2. Check no `locked` line in the helper log, `lantern status` view not locked, and no Lantern
     notification titled "Unlock" (`busctl` monitor of `Notify` calls for 5 s).
- [ ] **Step 6: Docs:**
  - `protocol.md`: the four messages and the PIN rules.
  - `README.md`: "Locked phones".
  - `HANDOFF.md`: move field finding 1 to resolved, and add the results of items 21–22.
- [ ] **Step 7:** Commit with `feat(tray): Unlock row; docs for the unlock prompt`.
