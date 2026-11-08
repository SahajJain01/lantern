# Lantern — handoff (end of Plan 1, 2026-11-08)

Start here in a new session. Read this file, then `README.md`, then the spec
`docs/superpowers/specs/2026-11-07-lantern-design.md`.

## Where things stand

Plan 1 of 3 (`docs/superpowers/plans/2026-11-07-lantern-core.md`) is **done**. Every task passed its
own review, and the whole-branch review passed after one round of fixes. The branch was merged into
`master`. There is no git remote yet (the proposed name is `SahajJain01/lantern`, still to create).

Lantern is **installed and running** on the PC: `~/.local/bin/lantern`, user unit `lanternd.service`
(enabled). When the phone is plugged in, its screen turns off, and `lantern open <pkg>` opens that app
as its own KDE window.

Acceptance (`tools/device_test.py`) is 8/8 PASS on Sahaj's phone:
- Items 1–7 run unattended. They pass with both `--signal=TERM` and `--signal=KILL`.
- Item 8 (USB unplug) was run with Sahaj on 2026-11-08 00:35:
  - On unplug, the phone's own cleanup turned the screen back on and restored `stay_on=15` within
    0.55 s.
  - After replugging, the session was ready again in 0.7 s.

## Layout

| Path | What |
|---|---|
| `android/helper/` | Java helper. It runs as the shell user via `app_process` and owns the virtual displays, encoders, input, power and settings. Built with `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py helper`; host-JVM tests run as part of the build. |
| `host/proto/` | Rust crate `lantern-proto`: the wire protocol v1 and golden vectors that both sides test against. |
| `host/lantern/` | Rust crate `lantern`. `lantern daemon` is lanternd: device tracking, helper deploy, session, window IPC. `lantern open <pkg>` is one process per window (winit/wgpu, VA-API decode). `lantern status` prints the daemon state. |
| `docs/protocol.md` | The wire spec. Keep it in step with both sides. |
| `docs/spikes/RESULTS.md` | Risk-spike measurements. Binding facts are folded into spec §9b. |
| `tools/build.py` | Targets: `helper`, `install`, `uninstall`. |
| `tools/device_test.py` | On-device acceptance suite: `--manual` adds item 8, `--only N…` runs selected items, `--state` prints the state. |

## Environment facts
- Phone: Xiaomi 11T Pro, serial `20e9318`, Android 17 / SDK 37, LineageOS 24, swipe keyguard, USB only.
- Also used: Sahaj's daily Motorola edge 50 pro, serial `ZD222LMDFV`, Android 16, users 0 and 10
  (Intune work profile); its `stay_on_while_plugged_in` original is `0`.
- Rust is in `~/.cargo/bin`, which is not on the agent tool's PATH. Prefix commands with
  `export PATH="$HOME/.cargo/bin:$PATH"`. Work in `host/` (`cargo test --workspace`); there are 143
  tests.
- The FFmpeg, VA-API, Wayland and clang dev headers are installed. The GPU is Intel (renderD128).
- Commit as `git -c user.name='Sahaj Jain' -c user.email='jainsahaj@gmail.com'`.
- What Lantern leaves on the phone:
  - Always: `/data/local/tmp/lantern-helper-<sha256>.jar`.
  - During a session: `/data/local/tmp/lantern-session.json`, a journal of the original
    `stay_on_while_plugged_in`, which is 15. While a session is live the setting reads 7 and the panel
    is off.
- Any helper or daemon death restores the phone, through the setsid Cleanup child and the persisted
  journal.

## Next: Plan 2 and Plan 3 (not yet written)

Write each plan with superpowers:writing-plans, using the spec as the authority, then run it with
superpowers:subagent-driven-development, as Plan 1 was.

- **Plan 2:**
  - KDE launcher entries and icons that follow the phone's apps (spec §7.1, §8.3). Window titles and
    labels currently show the package name.
  - Hidden-IME text input, including Vietnamese composition. The spike proved this in the probe app.
    Restoring the IME after a USB unplug must stay airtight.
  - Text clipboard, both directions (§8.6).
- **Plan 3:**
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

Still unverified: PIN keyguard, third-party notification clicks, IME text in real apps, rotation
started by the app on a real device, long runs with 5–6 encoders, and interactive play in games.

## Android users / work profiles (`lantern open --user N <pkg>`, 2026-11-08)
- CLI `--user N` (default 0; usage errors exit 2). The window sends `open.user`; app_id is
  `lantern.<pkg>` for user 0 (unchanged) and `lantern.<pkg>-u<N>` otherwise (`-` cannot occur in a
  package name, so no collision); title `<pkg> (user N)`; `windows.json` key `<pkg>` / `<pkg>:<N>`.
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
  - `LEGACY`: user 0 when any hidden user/package check throws (other Android releases):
    `getLaunchIntentForPackage`, as before user support.
  - `running` uses `isUserRunning(user, FLAG_AND_UNLOCKED)`, so a RUNNING_LOCKED user is refused.
  - Task lookup/close/app-exit are by display (`getTasks(…, displayId)` / `removeTask`); the shell
    uid sees and removes user-10 tasks even under `no_debugging_features`.
- **Device result, Motorola edge 50 pro (`ZD222LMDFV`, Android 16), user 10 = Intune work profile
  with `no_debugging_features`:** `am start --user 10` / `pm list packages --user 10` fail ("Shell
  does not have permission to access user 10"), but `lantern open --user 10
  com.google.android.apps.nbu.files` opens work Files in its own KDE window
  (`lantern.com.google.android.apps.nbu.files-u10`, title `… (user 10)`): helper log
  `route=LAUNCHER_APPS`, `dumpsys activity`: `Display #19` → `Task #18475 … U=10`,
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
  - **No phone:** with no session, `open` gets `error{"phone not connected"}`. When a session ends,
    every window closes.
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

## Known minor issues (deferred; none affect the phone's safety)
- **Phone safety, worth doing early in Plan 2**
  - A persisted journal original wins even if the user changed the setting since. Trust it only while
    the current value is 7.
  - The `pgrep` that looks for old helpers is too loose. Anchor it to
    `app_process … dev.lantern.helper.(Main|Cleanup)`.
  - Uninstall doesn't delete `lantern-session.json`.
  - There is no directory fsync after the journal rename.
  - Pruning old jars runs before the wait for old helpers to exit.
- **Helper**
  - `taskVanished` may fire wrongly for trampoline apps; require 2 empty polls. Promote this if a
    daily app "opens then closes".
  - `singleTask` apps can pull the user's phone task onto the Lantern display.
  - A failed resize or rotation leaves a frozen window with no error; it should report
    `streamFailed`.
  - Mouse and touch are classified per event, not per gesture.
  - `Input.states` is never cleared.
  - Negative stream ids are accepted.
  - The Cleanup restore isn't retried, and `DevicePower` acts on every display.
- **Daemon**
  - `Bye` is rarely sent.
  - Race between picking a free port and `adb forward` (use `tcp:0`).
  - A second SIGINT during teardown is ignored; teardown can take up to about 16 s.
  - A synchronous adb call in `Drop` can stall a worker for up to 2 s.
  - Stale-socket detection, and no `flock` to stop two daemons running.
  - ANSI colour codes end up in the journal.
  - A personal fallback serial `20e9318` remains in `select_serial`.
- **Window**
  - On focus loss, keys are released in press order instead of reverse order, and the synthetic key-up
    fires key-up actions.
  - A scale-only change doesn't send the new dpi.
  - Decoding runs on the reader thread.
  - `windows.json.lock` is left behind.
- **Tests**
  - Several tests rely on fixed sleeps (fake_adb, fake_helper, ipc).
  - `cli.rs`'s status test has an ETXTBSY risk.
  - The umask stress test is probabilistic.
  - A fresh checkout's `cargo test` needs the helper jar built first.
  - `device_test`'s final cleanup isn't guarded per window.
