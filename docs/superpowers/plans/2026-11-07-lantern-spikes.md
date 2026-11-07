# Lantern Risk Spikes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Answer the spec's §9 risks 1–7 on the real Xiaomi with throwaway probes, and write the answers up
so the full build plan can be written against facts.

**Architecture:** Two throwaway artifacts plus stock scrcpy 5.0.1:
- **probe APK** (`dev.lantern.probe`): a test activity, a stub IME, a notification listener and a command
  receiver.
- **probe-shell jar**, run as the shell user via `app_process`: owns its own virtual displays and encoders
  and calls hidden APIs by reflection.

Each spike ends in a verdict with numbers in `docs/spikes/RESULTS.md`.

**Tech Stack:** Java (javac `--release 11`, d8, aapt2, apksigner from `~/.local/opt/pd-toolchain`), Python 3
build/driver scripts, scrcpy 5.0.1, ffmpeg/ffprobe, adb.

**Spec:** `docs/superpowers/specs/2026-11-07-lantern-design.md` (§9 Risks, §9a, §5 Constraints).

## Global Constraints

- Device: Xiaomi 11T Pro, adb serial `20e9318`, LineageOS 23 / Android 16 (SDK 36). Only USB debugging
  enabled. **No root**, no freeform/desktop developer flags, no reboot.
- All spike code lives under `spikes/` and is **throwaway**: no tests, not imported by the real build.
  Learnings go to `RESULTS.md`, not code reuse.
- Tools: `adb` and `scrcpy` from `~/.local/bin` (scrcpy 5.0.1, server at
  `~/.local/opt/scrcpy-linux-x86_64-v5.0.1/scrcpy-server`). Toolchain env: `source ~/.local/opt/pd-toolchain/env.sh`.
- Every phone setting a spike changes is recorded first and restored by the same task. Task 7 diffs the
  phone against the Task 0 baseline. No changes stay on the phone except the probe APK, and it is
  uninstalled at the end.
- Hidden-API calls go through one reflection helper that, on failure, logs the class's real method
  signatures (`getDeclaredMethods`) so the result is still informative.
- Probe output: a single logcat tag `LanternProbe`, one `key=value` line per event. The driver scripts parse
  it.

## Review Focus

1. **Phone left modified after a spike aborts midway** (IME switched, panel off, stay-awake). Every
   driver uses `try/finally` restore. Task 7's baseline diff is the gate.
2. **Phone locked or screen off when a spike starts.** Drivers check `dumpsys window | grep mDreamingLockscreen`
   / `isKeyguardShowing` first and abort with a clear message, except in Task 3 where it is the point.
3. **adb disconnects mid-spike.** Drivers detect `adb` failure and exit non-zero, never hang. The probe-shell
   process must exit within 5 s of its stdin/socket closing; check this in Task 4's restore test.
4. **Display IDs are not stable.** Never hard-code a display ID; always read it from the probe-shell
   `DISPLAY id=` line.
5. **Hidden API missing or renamed on SDK 36.** A reflection failure is a *result* (record the real
   signatures), not a crash that loses the run.

---

### Task 0: Scaffold, baseline and probe artifacts

**Files:**
- Create: `spikes/build.py`: builds `spikes/out/probe.apk` and `spikes/out/probe-shell.jar`
- Create: `spikes/common.py`: adb wrapper, logcat reader, settings snapshot/diff
- Create: `spikes/probe-app/AndroidManifest.xml`, `spikes/probe-app/src/dev/lantern/probe/*.java`
- Create: `spikes/probe-shell/src/dev/lantern/probeshell/*.java`
- Create: `spikes/baseline/` (snapshot output, gitignored), `.gitignore`

**Interfaces (Produces):**
- `common.py`:
  - `adb(*args, check=True) -> str` (always `-s 20e9318`)
  - `snapshot() -> dict` (global/secure/system settings + `default_input_method` + `stay_on_while_plugged_in` + installed packages)
  - `diff(a: dict, b: dict) -> list[str]`
  - `logcat_lines(timeout_s) -> Iterator[dict]` (parses `LanternProbe` `key=value` lines)
  - `shell_probe(*args) -> subprocess.Popen` (pushes the jar to `/data/local/tmp/lantern-probe.jar` and runs `CLASSPATH=... app_process / dev.lantern.probeshell.Main <args>`)
- probe APK components:
  - `AnimActivity`, `documentLaunchMode="always"`, `resizeableActivity="true"`: full-screen frame counter (big number + moving bar) at display refresh, plus an `EditText`. It logs `event=resumed display=<id> task=<id>`, `event=text display=<id> value=<text>`, and `event=key display=<id> code=<n>`.
  - `ProbeIme` (InputMethodService with no visible input view: `onEvaluateInputViewShown()` returns false). It logs `event=ime_start display=<id> pkg=<editor pkg>`.
  - `ProbeNls` (NotificationListenerService). It stores `StatusBarNotification` by key.
  - `CmdReceiver`, exported broadcast receiver for action `dev.lantern.probe.CMD` with string extra `cmd`. Commands: `notify` (post a test notification whose contentIntent opens `AnimActivity` with extra `from=notif`), `fire key=<k> display=<id>`, `commit text=<t>`, `compose text=<t>`, `finish_compose`, `arm_restore prev=<ime id>`.
- probe-shell commands (Task-specific behaviour is defined in each task): `ping`, `vd create w h dpi flags=<names> [record=<path>] [fps=60] [codec=hevc|h264]`, `vd resize-bench`, `focus-bench`, `ime-policy`.
- probe-shell prints `DISPLAY id=<n>` when it creates a display, and `OK`/`ERR <msg>` per command.

- [ ] **Step 1:** Write `spikes/build.py` using PhoneDesk's pipeline (`~/repos/phone-desk/tools/build.py` `build_apk()`): aapt2 compile/link → javac `--release 11 -cp android.jar` → d8 `--min-api 30` → zipalign → apksigner with a throwaway key at `spikes/.local/probe.keystore` (gitignored). For the jar: javac + d8 into `classes.dex`, zipped as `probe-shell.jar`.
- [ ] **Step 2:** Implement the minimal `AnimActivity`, `CmdReceiver` and probe-shell `ping`, which prints `OK sdk=<SDK_INT> model=<Build.MODEL>`.
- [ ] **Step 3:** Run `python3 spikes/build.py && python3 -c "import spikes.common as c; print(c.adb('shell','echo','hi'))"`. Expected: the APK and jar are built, and `hi` is printed.
- [ ] **Step 4:** Take the baseline: `python3 spikes/snapshot.py save spikes/baseline/before.json` (thin CLI over `snapshot()`). Expected: the file exists and `default_input_method` is set.
- [ ] **Step 5:** Install the APK, run `shell_probe("ping")`, and launch AnimActivity on the main display. Expected: `OK sdk=36`, then a `LanternProbe event=resumed display=0` line.
- [ ] **Step 6:** Run `scrcpy -s 20e9318 --list-encoders` and save the output to `docs/spikes/encoders.txt`. Commit.

### Task 1: Encoder concurrency (§9.1)

**Files:** Create: `spikes/s1_encoders.py`

**Interfaces:** Consumes `AnimActivity` and `common.adb`. It produces numbers only.

- [ ] **Step 1:** Write `s1_encoders.py`. It accepts `--n`, `--size` (default `1280x800`), `--codec` (`h265` and `h264`) and `--fps 60`. It starts N stock scrcpy processes concurrently, each `scrcpy -s 20e9318 --new-display=<size> --start-app=dev.lantern.probe --no-window --no-audio --video-codec=<c> --max-fps=60 --time-limit=30 --record=<scratch>/s1_<i>.mkv`. Then it runs `ffprobe -count_frames` on each file and prints `n=<N> codec=<c> i=<i> fps=<frames/duration>`, plus per-run any scrcpy stderr containing `encoder`/`MediaCodec` errors.
- [ ] **Step 2:** Run it for N = 1..6 with both codecs (the phone screen is off via `adb shell input keyevent 223` before each run, then woken after; the phone must not lock, so set the screen timeout high and restore it afterwards). Expected output: one table.
- [ ] **Step 3:** Repeat N = 3 at `1920x1080`.
- [ ] **Step 4:** Write a `## S1 Encoders` section in `docs/spikes/RESULTS.md`. Give the table, then the verdict. **Pass:** at least 3 concurrent at 1280×800 with median at least 45 fps each on the better codec. Record which codec is better and the first N where any stream drops below 30 fps or errors. Commit.

### Task 2: Virtual-display resize cost (§9.6)

**Files:** Create: `spikes/probe-shell/src/dev/lantern/probeshell/VdBench.java`, `spikes/s6_resize.py`

**Interfaces:** Produces the probe-shell `vd resize-bench sizes=<WxH,...> reps=<n> codec=<c>`.

- [ ] **Step 1:** Implement `VdBench`. It creates a virtual display via `DisplayManagerGlobal`/`DisplayManager.createVirtualDisplay` as shell, with flags PUBLIC|OWN_CONTENT_ONLY|TRUSTED|OWN_DISPLAY_GROUP|OWN_FOCUS where accepted (look up the hidden flag constants in AOSP `DisplayManager` for SDK 36, and log which flags were accepted). It feeds a MediaCodec surface encoder, launches `AnimActivity` there with `ActivityOptions.setLaunchDisplayId`, then cycles through the sizes. For each resize it calls `VirtualDisplay.resize(w,h,dpi)`, then tears the encoder down and recreates it at the new size (also try `MediaCodec` reconfigure without recreation, if possible, and report both). It logs `resize ms_to_first_frame=<t> ms_to_app_relayout=<t>`. `app_relayout` is when `AnimActivity` logs `event=resumed`/a new `config w= h=` line (add a config-change log to AnimActivity).
- [ ] **Step 2:** Run `s6_resize.py` with 20 reps over `1280x800,1600x900,1000x1000,800x1280`. Expected: a per-size table with min/median/p95.
- [ ] **Step 3:** In RESULTS.md `## S6 Resize`, record the table and verdict. **Pass:** median `ms_to_first_frame` ≤ 150 ms and no encoder errors across 80 resizes. Also record the flags combination that worked; this is input for Tasks 3 and 4. Commit.

### Task 3: Locked phone (§9.2)

**Files:** Create: `spikes/s2_locked.py`

**Interfaces:** Consumes the probe-shell `vd create ... flags=<names> record=<path>` (extend `VdBench` with a single-display record mode).

- [ ] **Step 1:** Write `s2_locked.py`. It asks Sahaj (stdin prompt) to set a screen lock if none is set, and records whether one was set so it can tell him to remove it at the end. It then locks the phone (`input keyevent 223`, wait 10 s), and for each flag set in [`S6 flags`, `S6 flags + ALWAYS_UNLOCKED`] runs `vd create 1280x800 320 ... record=/data/local/tmp/s2_<k>.mp4` for 8 s with `AnimActivity` launched on it. It pulls the file and extracts frame 200 to `docs/spikes/s2_<k>.png` with ffmpeg.
- [ ] **Step 2:** Also run stock `scrcpy --new-display=1280x800 --start-app=dev.lantern.probe --no-window --record=...` while locked, as a comparison (shows whether scrcpy 5 already solves it).
- [ ] **Step 3:** Inspect the PNGs. **Pass:** for some flag set, the frame shows AnimActivity's counter advancing (compare frames 100 and 200), not the keyguard or black. Record whether `ALWAYS_UNLOCKED` was accepted for the shell uid or threw `SecurityException`. Commit.

### Task 4: Focus routing for keys and gamepads (§9.4)

**Files:** Create: `spikes/probe-shell/src/dev/lantern/probeshell/FocusBench.java`, `spikes/s4_focus.py`

**Interfaces:** Produces the probe-shell `focus-bench method=<m> reps=<n>`.

- [ ] **Step 1:** Implement `FocusBench`. It creates two displays A and B (S6 flags, with and without `OWN_FOCUS`), each running an `AnimActivity` task with its `EditText` focused. For each rep it picks a target and moves focus with method `m`:
  - `settask`: `IActivityTaskManager.setFocusedTask(taskId)`
  - `tap`: inject `ACTION_DOWN/UP` at a corner of the target display with `setDisplayId`
  - `toptask`: `moveTaskToFront`, or `IWindowManager`/`InputManager` "focused display" APIs found on SDK 36 (enumerate and record what exists)

  It then injects `KEYCODE_A` with **display ID `INVALID_DISPLAY` (-1)**, which is how a UHID gamepad/keyboard event arrives, and reads which display's AnimActivity logged `event=key`. It logs `method=<m> rep=<i> target=<A|B> got=<A|B|none> ms=<t>`.
- [ ] **Step 2:** Run a UHID check with a real pad. Keep `focus-bench` holding focus on the target (`method=<best> hold=1`). Run `scrcpy -s 20e9318 --display-id=<target> --gamepad=uhid --no-audio` (a window is needed for SDL to read the pad; keep it focused on the PC). Ask Sahaj to press A on a PC gamepad. Record which display's AnimActivity logged `event=key` with a gamepad keycode. Then switch focus to the other display and repeat. If no gamepad is at hand, record "UHID unverified".
- [ ] **Step 3:** Run `s4_focus.py --reps 20` for each method. **Pass:** some method routes 20/20 to the target within 100 ms. The probe-shell exits within 5 s after the driver closes its stdin (Review Focus 3). Record the result in RESULTS.md `## S4 Focus` and commit.

### Task 5: Hidden IME, Unicode text and self-restore (§9.5)

**Files:** Modify: `ProbeIme.java`, `CmdReceiver.java`. Create: `spikes/s5_ime.py`

**Interfaces:** Consumes `CmdReceiver` commands `commit`, `compose`, `finish_compose`, `arm_restore`, and probe-shell `ime-policy display=<id> policy=<LOCAL|FALLBACK|HIDE>` (via `IWindowManager.setDisplayImePolicy`).

- [ ] **Step 1:** Implement in `ProbeIme`: `commitText` and `setComposingText`/`finishComposingText` on the current `InputConnection`. For `arm_restore prev=<id>`, it binds a heartbeat (`CmdReceiver` receives `heartbeat` broadcasts every 1 s from the driver via `adb shell am broadcast`). After 5 s with no heartbeat, it writes `Settings.Secure.DEFAULT_INPUT_METHOD=<prev>` (needs `pm grant dev.lantern.probe android.permission.WRITE_SECURE_SETTINGS`) and logs `event=ime_restored`.
- [ ] **Step 2:** Write `s5_ime.py`. It saves the current `default_input_method`, then runs `ime enable` / `ime set dev.lantern.probe/.ProbeIme` and creates a display running AnimActivity. For each policy in LOCAL, FALLBACK and HIDE, it focuses the EditText and sends:
  - `commit text="Xin chào, Tiếng Việt ạ 👋"`
  - `compose text="việ"` then `finish_compose`

  It checks the `event=text` value equals the expected string exactly. It captures a frame of the virtual display and a `screencap` of display 0 to check that no keyboard is drawn.
- [ ] **Step 3:** Repeat Step 2's text check in two real apps on the virtual display: Chrome's address bar and one messaging app Sahaj uses (driver prompts for the package). Verify by screenshot.
- [ ] **Step 4:** Restore test. Arm the restore, stop the heartbeats and unplug-equivalent (`adb disconnect` is not applicable over USB, so kill the driver and ask Sahaj to pull the cable). After 10 s, reconnect and read `default_input_method`. **Pass:** it equals the saved original. Afterwards the driver restores in `finally` anyway.
- [ ] **Step 5:** In RESULTS.md `## S5 IME`, record which policy delivers exact Unicode with nothing visible, per app, and the restore result. Commit.

### Task 6: Notification click onto a Lantern display (§9.3) and app compatibility (§9.7)

**Files:** Modify: `ProbeNls.java`, `CmdReceiver.java`. Create: `spikes/s3_notif.py`, `spikes/s7_compat.py`

- [ ] **Step 1:** Implement `fire key=<k> display=<id>` in `ProbeNls`. It sends the stored notification's `contentIntent` with `ActivityOptions.makeBasic().setLaunchDisplayId(id)`, plus `setPendingIntentBackgroundActivityStartMode(MODE_BACKGROUND_ACTIVITY_START_ALLOWED)` on SDK ≥ 34. It logs the result or exception.
- [ ] **Step 2:** Write `s3_notif.py`. It runs `cmd notification allow_listener dev.lantern.probe/.ProbeNls`, creates a display (S6 flags) via probe-shell, posts `notify`, and fires it at the display. Then it checks that AnimActivity logs `event=resumed display=<vd>` with `from=notif`. It repeats for a real app notification: the driver asks Sahaj to trigger one (for example, a message from another device) and lists stored keys to pick from. Verify where it landed with `dumpsys activity activities | grep -E 'displayId|<pkg>'`. Finally it revokes the listener (`disallow_listener`).
- [ ] **Step 3:** Record pass/fail per case in RESULTS.md `## S3 Notification click`. **Pass:** both land on the Lantern display. If the real-app case lands on display 0, also record whether the fallback works (launching the package's launcher activity on the display from probe-shell).
- [ ] **Step 4:** Write `s7_compat.py`. For each launcher package from `cmd package query-activities -a android.intent.action.MAIN -c android.intent.category.LAUNCHER --brief` (third-party plus Chrome), it runs stock `scrcpy --new-display=1280x800 --start-app=<pkg> --no-window --no-audio --time-limit=8 --record=<scratch>/<pkg>.mkv`. It extracts the last frame and classifies it as `ok` (non-uniform image), `black` (uniform dark) or `error` (scrcpy failed). It writes `docs/spikes/compat.csv` and a contact sheet `docs/spikes/compat.png` (ffmpeg tile).
- [ ] **Step 5:** Review the contact sheet. Correct misclassifications by hand in the CSV, and summarize in RESULTS.md `## S7 Compatibility`. Commit.

### Task 7: Restore check and write-up

**Files:** Modify: `docs/spikes/RESULTS.md`. Create: `spikes/baseline/after.json`

- [ ] **Step 1:** Uninstall `dev.lantern.probe`, remove `/data/local/tmp/lantern-probe.jar` and spike recordings, and confirm that `ime`, `notification listener` and screen timeout are restored.
- [ ] **Step 2:** Run `python3 spikes/snapshot.py save spikes/baseline/after.json && python3 spikes/snapshot.py diff spikes/baseline/before.json spikes/baseline/after.json`. Expected: empty diff, except keys Android itself churns (list them explicitly, e.g. timestamps).
- [ ] **Step 3:** Add a top `## Summary` to RESULTS.md: one line per risk with pass/fail, and a **Design impact** list naming each §9/§9a decision that a result confirms or changes. Examples: the codec choice, the display flag set, the focus method, the IME policy, the notification fallback, and the max-windows number.
- [ ] **Step 4:** Commit and report to Sahaj. The next step is writing the full build plan against these results.
