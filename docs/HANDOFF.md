# Lantern — handoff (end of Plan 2, 2026-11-10)

Start here in a new session. Read this file, then `README.md`, then the spec
`docs/superpowers/specs/2026-11-07-lantern-design.md`.

## Where things stand

- **Plan 1** (`docs/superpowers/plans/2026-11-07-lantern-core.md`) is done and merged into
  `master`.
- **Plan 2** (`docs/superpowers/plans/2026-11-10-lantern-desktop-integration.md`) is **done,
  pending the deferred device checks below**. All 10 tasks passed their own reviews (Tasks 3, 4, 7,
  8 and 9 after one fix round each). The work is on branch `plan2-desktop-integration` (worktree
  `.worktrees/plan2`); the whole-branch review and the merge into `master` are still to do. The
  SDD ledger and per-task reports are in `.superpowers/sdd/2026-11-10-lantern-desktop-integration/`.
- There is no git remote yet (the proposed name is `SahajJain01/lantern`, still to create).

Lantern is **installed and running** on the PC from the Plan 2 build: `~/.local/bin/lantern`, user
unit `lanternd.service` (enabled). When the phone is plugged in, its screen turns off, the Lantern
keyboard is selected, and its apps are in the KDE menu under **Phone**.

### Plan 2 acceptance (`tools/device_test.py`, Xiaomi 20e9318, 2026-11-10)

The Plasma session was **locked** (Sahaj away), and a locked session sends windows no frames, so
the window items ran in a private headless KWin (`--nested`, new in Task 10). The Motorola was not
attached.

- `--nested`: **12/12 automatic PASS**, three runs (one with `--signal=KILL`). Item 12 read
  `text=Tiếng Việt 👍việ`; item 13 got the phone's copy into Klipper 0.04 s after Ctrl+C.
- Without `--nested` on the locked desktop: 7/12. Items 4, 6, 12 and 13 fail only because the
  window gets no frames (no configure acks, debug steps never start), and item 5 fails because item
  4 killed its window. 1–3, 7 and 9–11 pass there.
- Not run (need Sahaj or the other phone): items 8 and 14 (`--manual`), and the Motorola check.

**Deferred checks, with the commands to run:**
1. Real desktop, unlocked, Sahaj at the PC:
   `python3 tools/build.py install && python3 tools/device_test.py` (expect 12/12 without
   `--nested`; items 12 and 13 then use real window focus instead of the `focus` debug step).
2. With Sahaj, cable in hand: `python3 tools/device_test.py --manual --only 8 14`.
3. Motorola edge 50 pro (work profile, user 10): with only it attached (or `LANTERN_SERIAL=<its
   serial>`), check the menu has "Files (work)" with the badged icon, open it from the menu
   (`lantern open --user 10 com.google.android.apps.nbu.files`), check the title is
   `Files (work)` and the taskbar icon is badged, and type into its search field.
4. Typing with a real PC input method: start ibus or fcitx5 with a Vietnamese engine, type
   `Tiếng Việt` into the companion's test screen (or any phone app) on the real desktop.

### Still unverified
- The Motorola work-profile catalogue, badged icons and the `Files (work)` title (Task 5/6/10).
- Typing on the real desktop: real focus, a physical `abc`, and any real PC input method (none
  runs on this PC: no ibus, no fcitx5). The composition path ran only through `LANTERN_DEBUG_INPUT`
  steps in a headless KWin.
- The keyboard restore after a USB yank (item 14). Helper death (`kill -9`) and a clean stop are
  device-verified (Cleanup restores LatinIME in about 0.6 s).
- The keyboard-less fallback session on a device (`ime set` refused → stay-awake-only session,
  IME policy HIDE). Host-JVM tested only; forcing it needs a hand-made settings change.
- The companion's watchdog switch on a device (link lost and nothing else restored the keyboard).
- A real password manager's secret hint (tested with a throwaway ext-data-control client, not
  KeePassXC).
- **Encoder crash on resize (observation, not fixed):** in Task 9, 1 of 3 window opens died on the
  first resize to 1024x740 in a nested KWin: `c2.qti.hevc.encoder` "work failed to complete: 14",
  then `video stream failed: fatal codec error: ... Pending dequeue output buffer request
  cancelled`. In Task 10 it did not recur: 0 codec errors in 23 window opens across six suite
  invocations (window logs and logcat checked). `DrainLoop` treats `IllegalStateException` as fatal; worth a look with Plan 1's "failed
  resize" item.
- From Plan 1: PIN keyguard, third-party notification clicks, app-started rotation on a real
  device, long runs with 5–6 encoders, interactive play in games.

## Layout

| Path | What |
|---|---|
| `android/helper/` | Java helper. It runs as the shell user via `app_process` and owns the virtual displays, encoders, input, power, settings, the companion link and the phone clipboard. Built with `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py helper`; host-JVM tests run as part of the build. |
| `android/companion/` | Companion APK `dev.lantern.companion`: `LinkProvider` (the link), `LanternIme` (the invisible keyboard), the launcher catalogue, `LinkReceiver` (test-only `TEST_ACTIVITY`) and the disabled `TestActivity`. Built without Gradle by `python3 tools/build.py companion`. |
| `host/proto/` | Rust crate `lantern-proto`: the wire protocol v1 and golden vectors that both sides test against. |
| `host/lantern/` | Rust crate `lantern`. `lantern daemon` is lanternd: device tracking, helper and companion deploy, session, window IPC, KDE menu catalogue (`daemon/catalog.rs`), PC clipboard (`clipboard/`). `lantern open <pkg>` is one process per window (winit/wgpu, VA-API decode, text input). `lantern status` prints the daemon state. `lantern undo-setup` reverses the phone setup. |
| `docs/protocol.md` | The wire spec, including the companion link. Keep it in step with both sides. |
| `docs/spikes/RESULTS.md` | Risk-spike measurements, plus the Plan 2 IME display policy finding. |
| `tools/build.py` | Targets: `helper`, `companion`, `all`, `install`, `uninstall` (runs `lantern undo-setup` best effort). |
| `tools/device_test.py` | On-device acceptance suite, items 1–14: `--manual` adds 8 and 14, `--only N…` (numbers or names), `--nested` (headless KWin for a locked desktop), `--state`. |

## Environment facts
- Phone: Xiaomi 11T Pro, serial `20e9318`, Android 17 / SDK 37, LineageOS 24, swipe keyguard, USB
  only. Its originals: `stay_on_while_plugged_in` 15, keyboard
  `com.android.inputmethod.latin/.LatinIME` (also the whole `enabled_input_methods`).
- Also used: a Motorola edge 50 pro, Android 16, users 0 and 10 (a managed work profile with
  `no_debugging_features`); its `stay_on_while_plugged_in` original is `0`.
- Rust is in `~/.cargo/bin`, which is not on the agent tool's PATH. Prefix commands with
  `export PATH="$HOME/.cargo/bin:$PATH"`. Work in `host/` (`cargo test --workspace`): 224 tests
  (1 ignored). The helper build runs 162 host-JVM tests, the companion build 32.
- The FFmpeg, VA-API, Wayland and clang dev headers are installed. The GPU is Intel (renderD128).
- The companion signing key is `~/.local/share/lantern/companion-signing.p12`
  (`LANTERN_COMPANION_KEY`); `build.py` pins its certificate into the helper (`CompanionPin`).
- Commit with the repo's usual author identity (`git log -1 --format='%an <%ae>'`).
- What Lantern leaves on the phone:
  - Always: `/data/local/tmp/lantern-helper-<sha256>.jar` and the companion app.
  - During a session: `/data/local/tmp/lantern-session.json`, a journal of the originals of
    `stay_on_while_plugged_in`, `enabled_input_methods` and `default_input_method`. While a
    session is live, stay-awake reads 7, the keyboard is `dev.lantern.companion/.LanternIme`
    (appended to the enabled list), and the panel is off.
- Any helper or daemon death restores the phone, through the setsid Cleanup child and the persisted
  journal; the companion's watchdog is the last line for the keyboard.
- A locked Plasma session gives windows no frames. Window checks then need `--nested` (or
  `kwin_wayland --virtual --no-lockscreen --socket <name>` and `WAYLAND_DISPLAY=<name>`); that
  compositor gives no keyboard focus, hence the `focus` debug step.

## Plan 2 architecture (decisions Sahaj may override)
- **Companion link.** An app cannot dial the helper (SELinux denies `connectto` on shell sockets;
  the network policy drops the companion's loopback TCP). The helper creates two pipes and hands
  them to the companion's shell-only provider (`content://dev.lantern.companion.link`, `call("link",
  token, {in, out})`, permission DUMP plus a caller-uid check). Before the call it verifies the
  provider's owner: package `dev.lantern.companion`, its uid in user 0, and exactly one signer
  equal to the pinned certificate (`CompanionTrust`). On any mismatch it releases the provider and
  runs without the companion. Holding the provider keeps the companion's process unfrozen. Any
  new phone-side IPC that carries tokens or text must keep that check.
- **Startup order.** The helper waits up to 3 s for the companion, then `Session.start`, then the
  hello reply, so lanternd allows 10 s for the reply. `ime_prev` (the original keyboard) is a
  link-only message sent after `Session.start`; the catalogue starts only on the link-only `sync`
  sent from the session-up hook, so no bulk data waits in the Router's pre-hello buffer.
- **Keyboard.** Journaled before any change, restored in reverse by the helper, by Cleanup on
  helper death, and by the companion's watchdog (5 s after the link is lost, with retries and a
  fallback keyboard). Each restore applies only while the setting still holds Lantern's value. If
  the switch fails, the session falls back to stay-awake only and reports
  `companion{unavailable, "keyboard switch failed: …"}`; a session never fails for the keyboard.
- **IME display policy rule.** Lantern displays use `LOCAL` only while the session selected the
  Lantern keyboard, `HIDE` otherwise. Under `HIDE` Android keeps the IME bound to the launcher on
  display 0 and commits never reach the app; under `LOCAL` any keyboard that draws would draw into
  the PC window. `AppDisplay.open` takes the session's `imeSelected`.
- **Text vs keys.** The window sends a key event when Ctrl/Alt/Super is held or the text equals
  what the US layout gives for that key; any other text goes as an `ime commit`, and preedit as
  `compose`/`finish`. lanternd forwards `ime` only up to `MAX_IME_TEXT`.
- **Test readback.** `TestActivity` (one EditText; logs `text=<content>`, tag `LanternTest`) is
  disabled except during tests. The shell cannot `pm enable` another package's component, so it is
  toggled with the companion's DUMP-only broadcast `am broadcast --user 0 -n
  dev.lantern.companion/.LinkReceiver -a dev.lantern.companion.TEST_ACTIVITY --es enabled
  true|false` (result 1 = done). Window debug steps (`LANTERN_DEBUG_INPUT`) gained `text:`,
  `compose:`, `finish`, `key:Ctrl+X` and `focus`.
- **Menu catalogue.** `lantern.<pkg>[-u<N>].desktop` entries in `~/.local/share/applications` with
  `Categories=X-Lantern` and ownership marker `X-Lantern-Serial`, gathered into a "Phone" submenu
  (`lantern.menu`, `lantern-phone.directory`). Prune only users that sent at least one app in a
  full sync whose count matched, so a paused work profile keeps its entries. Window titles use the
  helper's `opened.label` plus " (work)" / " (user N)"; the app_id is unchanged.
- **Clipboard.** lanternd reads and sets the PC clipboard through `ext_data_control_v1` on its own
  thread; the helper listens on a `lantern-clip` HandlerThread (with `ClipboardManager.mHandler`
  redirected by reflection). Each side suppresses only the echo of what it last received, and
  any other local change clears that; the helper dedupes Android's double listener fire by clip
  timestamp. KDE secret hints and Android `IS_SENSITIVE` clips are never synced; no text is logged.
- **`lantern undo-setup`.** Refuses whenever lanternd answers on its socket, in any state (idle
  includes retry backoff). Otherwise it runs the helper's `--forget` (restores any journaled
  setting that still holds Lantern's value, deletes Lantern's files in `/data/local/tmp`),
  uninstalls the companion, removes that serial's menu entries and icons, and deletes the device
  record (kept if a phone step failed). Exit 0 only if every step succeeded.
- **Companion install.** lanternd installs or updates the companion before each session and shows
  "Lantern set up your phone" the first time (record in `~/.local/state/lantern/devices/`). A
  failed install leaves a session without the companion and one notification per daemon run.

## Next: Plan 3 (not yet written)

Write it with superpowers:writing-plans, using the spec as the authority, then run it with
superpowers:subagent-driven-development, as Plans 1 and 2 were. Finish Plan 2's deferred checks
and the merge first.

- Notifications to KDE, with clicks opening that app's window (§8.5).
- Drag and drop from PC to phone: copy the file, then use Android share, falling back to Downloads.
- One phone audio stream.
- Gamepads: the helper must re-attach the UHID controller to the display of the active PC window.
  Spikes showed scrcpy keeps it pinned to the first display. On Linux the GameSir must be held
  open, or it reconnects over and over.
- A tray icon.
- Pausing a window's encoder while it is minimised. This is added within protocol v1 (see
  `docs/protocol.md`).
- A latency harness, and the target from spec §9a/§11: no more than 5 ms worse than scrcpy.

App note: in Microsoft Windows App (`com.microsoft.rdc.androidx`), when a remote session is
letterboxed (its resolution differs from the window's aspect), the cursor is offset by the width of
the black strip. That is Windows App's own mapping bug. Fix: set the connection's display resolution
to "Match current display". If that ever isn't enough, add an optional per-app `--aspect W:H` to
Lantern, so the phone display keeps that aspect and the PC window letterboxes instead.

## Android users / work profiles (`lantern open --user N <pkg>`, 2026-11-08)
- CLI `--user N` (default 0; usage errors exit 2). The window sends `open.user`; app_id is
  `lantern.<pkg>` for user 0 (unchanged) and `lantern.<pkg>-u<N>` otherwise (`-` cannot occur in a
  package name, so no collision); title `<label> (work)` for a managed profile, `<label> (user N)`
  for another user (`<pkg>` when the phone sent no label); `windows.json` key `<pkg>` / `<pkg>:<N>`.
  Single-instance focus keys on pkg+user (test `single_instance_is_per_pkg_and_user`).
- CLI rejects package names outside `[A-Za-z0-9_.]` and `--user` values with a sign or leading
  zeros (exit 2).
- Helper: `UserLaunch` (pure policy, host-tested with a fake) over `DeviceUsers` (IUserManager,
  IActivityManager, IPackageManager, ILauncherApps). Resolves before any display is created and
  picks a route:
  - `DIRECT`: launcher activity from that user's PM, `IActivityManager.startActivityAsUser(…, user)`
    with an explicit component and `MULTIPLE_TASK`.
  - `LAUNCHER_APPS`: when the helper is the shell uid (2000; root skips the check) and the user has
    `no_debugging_features`. Requires the user in `ILauncherApps.getUserProfiles()` (the current
    user's profile group: work profile, private space, clone), resolves with
    `getLauncherActivities("com.android.shell", pkg, user)` and starts with
    `ILauncherApps.startActivityAsUser(null, "com.android.shell", null, component, null,
    ActivityOptions.setLaunchDisplayId(id), user)`. LauncherAppsService checks the profile group,
    not the shell's access to the user, so the MDM restriction does not apply. The intent is built
    by LauncherApps (`NEW_TASK|RESET_TASK_IF_NEEDED`, no `MULTIPLE_TASK`).
    `LaunchTasks` (pure, host-tested) records that app+user's tasks on all displays
    (`getTasks(…, INVALID_DISPLAY)`) before launching: a pre-existing task that lands on the Lantern
    display is reported as `opened.moved` (the window prints one stderr line); if none lands within
    3 s, only tasks NOT in the record are removed (e.g. one that started on display 0). Both routes
    do this (singleTask apps reuse their task even with `MULTIPLE_TASK`).
  - `LEGACY`: user 0 when any hidden user/package check throws (other Android releases):
    `getLaunchIntentForPackage`, as before user support.
  - `running` uses `isUserRunning(user, FLAG_AND_UNLOCKED)`, so a RUNNING_LOCKED user is refused.
  - Task lookup/close/app-exit are by display (`getTasks(…, displayId)` / `removeTask`); the shell
    uid sees and removes user-10 tasks even under `no_debugging_features`.
- **Device result, Motorola edge 50 pro (Android 16), user 10 = managed (MDM) work profile
  with `no_debugging_features`:** `am start --user 10` / `pm list packages --user 10` fail ("Shell
  does not have permission to access user 10"), but `lantern open --user 10
  com.google.android.apps.nbu.files` opens work Files in its own KDE window
  (`lantern.com.google.android.apps.nbu.files-u10`; with Plan 1 the title was `… (user 10)`):
  helper log `route=LAUNCHER_APPS`, `dumpsys activity`: `Display #19` → `Task #18475 … U=10`,
  `launchedFromPackage=com.android.shell userId=10`. Closing the KWin window → `removeTask 18475 ->
  true`, 0 user-10 Files tasks and 0 lantern displays left. User 0 Settings still uses `DIRECT`.
  Spike details: `.superpowers/work-profile-spike.md`.
- Still unverified: a second open of a profile app that already has a task on the phone's screen
  (LauncherApps may move that task onto the Lantern display, and closing the window then quits
  it); a profile with a separate work challenge that is currently locked; private space/clone
  profiles; the `LEGACY` path on a real device (host-tested only).

## Decisions made during Plan 1 (Sahaj may override)
- **Helper (phone side)**
  - **Panel off:** uses `SurfaceControl.setDisplayPowerMode`, as scrcpy does, because
    `DisplayManager.requestDisplayPower` doesn't exist on SDK 37.
  - **Session start:** the phone is changed only after a valid `hello`; a wrong token changes nothing.
    The helper replies `hello{version:1, token:""}`.
  - **Version mismatch:** the helper sends `error{code:"version"}`; the daemon then exits with code 3,
    and the unit has `RestartPreventExitStatus=3`.
- **Daemon**
  - **Window ids:** the daemon assigns window id N and forwards `open` on stream N. Every later frame
    carries N. Ids start again at 1 each session.
  - **Activation tokens:** the token reaches the existing window as `focus{activation_token}`. Both
    `focus` and `open` carry it as an optional field, so v1 bytes are unchanged, and the daemon strips
    it before forwarding to the helper.
  - **No phone:** with no session, `open` gets `error{"phone not connected"}` (the window then shows
    "Connect your phone"). When a session ends, every window closes.
  - **Slow windows:** each window client has a queue of 240 frames. A client that overflows it is
    dropped, and its app quits. The daemon never blocks reading from the helper.
- **Window process**
  - **Display size:** `display_request` uses the exact physical pixels, with no rounding to 16; the
    encoder handles alignment.
  - **Rotation:** a rotated window keeps its height, which differs from the spec's "long edge" wording.
    Portrait apps open at the remembered size or 1280×800 and then follow the app's rotation.
  - **Colour:** the colour matrix follows the stream's signalled colourspace. The phone sends BT.601
    limited range.
  - **Process lifetime:** `lantern open` stays alive as the window. If the app is already open, it
    exits 0 after focusing that window.
- **Packaging:** the unit uses `Restart=on-failure`, because SIGTERM means an operator stop.

## Known minor issues (deferred; none leaves the phone changed)
Plan 1's phone-safety items (journal trust, anchored `pgrep`, journal on uninstall, directory
fsync, prune after the wait) were fixed in Plan 2 Task 1.

- **Helper**
  - `taskVanished` may fire wrongly for trampoline apps; require 2 empty polls. Promote this if a
    daily app "opens then closes".
  - `singleTask` apps can pull the user's phone task onto the Lantern display.
  - A failed resize or rotation can leave a frozen window; see also the encoder crash above.
  - Mouse and touch are classified per event, not per gesture. `Input.states` is never cleared.
    Negative stream ids are accepted.
  - The Cleanup restore isn't retried, and `DevicePower` acts on every display.
  - Journal: a failed save can leave a journal behind; lenient int parsing; the `--forget`
    restore-throws branch is untested.
  - Companion link: a link that arrives after the 3 s deadline is accepted although lanternd was
    already told `unavailable`; `provider()` catches don't cover `Error` subclasses (release
    skipped); a helper/companion key mismatch shows only as a silent `unavailable`; the Router's
    pre-hello buffer is still unbounded in principle (bulk data no longer goes through it).
  - Catalogue: the labels map is never pruned on resync; the companion doesn't bound label size
    or validate pkg (the host does); a profile toggle triggers two full syncs; the receiver can
    leak if `registerReceiver` throws; `render()` doesn't catch OOM.
  - Keyboard: the fallback triggers on any clean-rollback failure (a misleading "keyboard switch
    failed: Cleanup not ready" is possible); `ime_restored` is logged for a switch that didn't
    throw but didn't take; the IMM binder call runs under the watchdog monitor; the fallback's
    hello reply takes about 9 s of lanternd's 10 s; the stale-armed watchdog if the hello write
    fails (benign); a mid-session keyboard change under `LOCAL` could draw into the video.
  - Clipboard: the `mHandler` reflection failing shows only in logcat (phone→PC then silently
    off); a clip timestamp of 0 disables the double-fire dedupe.
- **Companion**
  - It logs every `ime` dropped before the IME is created; `LinkReceiver` is misnamed (it only
    toggles TestActivity).
- **Daemon**
  - `Bye` is rarely sent. Race between picking a free port and `adb forward` (use `tcp:0`).
  - A second SIGINT during teardown is ignored; teardown can take up to about 16 s.
  - A synchronous adb call in `Drop` can stall a worker for up to 2 s.
  - No `flock` to stop two daemons running. ANSI colour codes end up in the journal.
  - A personal fallback serial `20e9318` remains in `select_serial`.
  - Companion install: `ensure()` has no overall timeout (worst case about 7 min before a session
    starts); a cancel leaves the pushed APK; retries re-push; the notice says "Installed" for an
    already-current companion with no record; `pm uninstall --user 0` leaves other users' copies;
    `adb devices` in undo-setup has no timeout; the "no daemon" message order in undo-setup.
  - Catalogue: file IO and base64 decoding run on the async loop; an odd serial that doesn't
    round-trip through `escape_value` would self-prune; the host doesn't cap icons at 1 MiB; a
    failed `kbuildsycoca6` spawn is silent; undo-setup rewrites the menu files; two phones'
    entries for the same app overwrite each other; a deleted profile's entries linger until
    undo-setup.
  - Clipboard: a stale own-selection offer can briefly bounce older text; Klipper re-owning a
    PC-originated text re-sends it (can overwrite an unsynced phone clip); a Y,Z burst before
    Y's echo can cascade (one-entry echo memory); the read job queue is unbounded; the
    `systemctl show-environment` call has no timeout and retries every 10 s when headless.
- **Window**
  - The phone display is the exact requested size, but the encoder captures only its top-left
    16-aligned part: up to 15 px at the right and bottom are never shown. Input is scaled to the
    captured part (fixed 55996ad; see `.superpowers/mouse-offset.md`). To show everything, request
    16-aligned displays, or encode at the exact size where the codec allows it.
  - On focus loss, keys are released in press order instead of reverse order, and the synthetic
    key-up fires key-up actions.
  - A scale-only change doesn't send the new dpi. Decoding runs on the reader thread.
    `windows.json.lock` is left behind.
  - Text: the first character after switching windows may be dropped (the Lantern IME is
    recreated per display); a dead key with Ctrl/Alt/Super is swallowed; CapsLock/NumLock
    characters go as text, not keys; the `LANTERN_DEBUG_INPUT` step log prints the typed test text.
  - "Connect your phone" also shows when lanternd isn't running at all.
- **Tests**
  - Several tests rely on fixed sleeps or timing windows (fake_adb, fake_helper, ipc, the catalogue
    debounce, CompanionLink); some assert little (`setExtrasAcceptsHandler`, the ipc mismatch step,
    the AltGr test, `clipboard_offer_without_session_is_dropped`); untested: the preedit state
    machine in `App`, the helper's icon blanking, the clipboard reader's secret path, a stale
    socket in `check_daemon`.
  - `cli.rs`'s status test has an ETXTBSY risk. The umask stress test is probabilistic.
  - A fresh checkout's `cargo test` needs the helper jar and companion APK built first.
  - `device_test.py` restores only the PC clipboard's text (Klipper's D-Bus), not other mime
    types.
