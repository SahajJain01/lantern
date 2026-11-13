# Lantern — handoff (end of Plan 3, 2026-11-13)

Start here in a new session. Read this file, then `README.md`, then the spec
`docs/superpowers/specs/2026-11-07-lantern-design.md`.

## Where things stand

- **Plan 1** (`docs/superpowers/plans/2026-11-07-lantern-core.md`) is done and merged into
  `main`.
- **Plan 2** (`docs/superpowers/plans/2026-11-10-lantern-desktop-integration.md`) is **done,
  pending the deferred device checks below**. Each of the 10 tasks passed its own review (Tasks 3,
  4, 7, 8 and 9 after one fix round each); the final whole-branch review then found 1 critical, 4
  important and 5 minor issues, all fixed in one wave (`final-findings.md` and
  `final-fix-report.md`). Branch `plan2-desktop-integration` was fast-forward merged into `main`
  (at `c151d37`) and its worktree removed; the SDD ledger and per-task reports went with the
  worktree. No record survives of a separate re-review of the fix wave (`f33642d..3050bf9`).
- **Plan 3** (`docs/superpowers/plans/2026-11-10-lantern-phones-and-tray.md`) is **implemented on
  branch `plan3-phones-and-tray`** (worktree `.worktrees/plan3`, base `main` `b926173`). Device
  steps were deferred during execution by Sahaj's instruction and run together at the end (see
  "Plan 3 device acceptance"). Tasks 1–7 each passed their own review (Tasks 6 and 7 after one fix
  round). The final whole-branch review found 1 important issue (a single `paused` slot let a
  disconnected phone restart) plus 1 minor; both fixed in `8c05253` and re-reviewed clean. Every
  other `minor (deferred)` line was triaged OK-TO-DEFER (listed under Known minor issues).
  Rebased onto `main` and fast-forward merged (`a865291`); the worktree was removed and the SDD
  ledger copied to the main checkout's gitignored `.superpowers/sdd/`.
  - Host: `cargo test --workspace --offline` 308 passed, 1 ignored (2026-11-13). Helper build:
    170 host-JVM tests, companion build: 41, all passing.
  - The SDD ledger and per-task reports are in the main checkout's gitignored
    `.superpowers/sdd/2026-11-10-lantern-phones-and-tray/`.
- The git remote is `origin` (`https://github.com/SahajJain01/lantern.git`, branch `main`).

Lantern is **installed and running** on the PC from the **Plan 3** branch build (2026-11-13):
`~/.local/bin/lantern`, user unit `lanternd.service` (enabled). When the phone is plugged in, its
screen turns off, the Lantern keyboard is selected, and its apps are in the KDE menu under
**Phone**.

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
- After the final-review fix wave: `--nested` **12/12 PASS** again (scratch `XDG_RUNTIME_DIR`,
  real-session KWin config mtimes and `kwin_wayland`/`ksecretd`/`xdg-desktop-portal-kde` pids
  unchanged). Also on the device: text bound to its window (TestActivity window on display A
  typed `alpha`, Settings then took the IME to display B, A's next `text:` was dropped with
  `ime op=commit dropped len=4: typed for display A, the keyboard is on display B`), a
  left-enabled TestActivity disabled on relink, and lanternd waiting for the previous helper
  before the companion update.
- Real desktop, unlocked (2026-11-10, after the merge): `python3 tools/build.py install &&
  python3 tools/device_test.py` **12/12 PASS without `--nested`**. Item 12 read
  `text=Tiếng Việt 👍việ` through real window focus; item 13 got the phone's copy into Klipper
  0.077 s after Ctrl+C; item 7 restored the phone 0.9 s after the kill.

### Plan 3 device acceptance (2026-11-13, both phones attached, desktop unlocked)

- `LANTERN_SERIAL=20e9318` set in the systemd user environment for the run (both phones were
  plugged in), then `python3 tools/build.py install && python3 tools/device_test.py`: **17/18**;
  item 20 failed on two test-script bugs (busctl took `-1` as an option; the tray's bus name was
  taken before `baseline()` restarted lanternd), fixed in `14f4a7c`; `--only 20` then **PASS**
  (paused 0.6 s after the tray's Disconnect, phone restored 0.4 s later, ready 1.3 s after
  Connect). So **18/18 automatic PASS**. The tray screenshot is in the item's log dir.
- Two-phone check (Review Focus 1), `LANTERN_SERIAL` unset again: with the Xiaomi served, the menu
  held 15 Xiaomi-only, 3 shared (`X-Lantern-Serials=20e9318;ZD222LMDFV;`) and 52 Motorola-only
  entries, including `Files (work)` / `Play Store (work)` / `Authenticator (work)`. `lantern
  disconnect` paused the Xiaomi (restored: LatinIME, stay-awake 15) and lanternd went straight on
  to the Motorola (`serve_next`); its record got `"name":"edge 50 pro"` (its `device_name`), and
  after its full sync the counts were unchanged (no Xiaomi entry pruned). `lantern open
  com.android.calculator2` (Xiaomi-only) then gave `app not on this phone` and `connect notice:
  Connect your Xiaomi 11T Pro`.
- Left as found by the run: the Motorola is served, the Xiaomi is paused (replug it or click
  Connect in the tray).
- Still to check by hand: open the tray menu on the Plasma panel and look at it.

To re-run: in the worktree, with the desktop **unlocked** (item 20 needs the real desktop and its
session bus; there is no `--nested` equivalent for it). `build.py install` restarts lanternd from
the Plan 3 build: the phone panel goes off and stay-awake reads 7.

```sh
cd .worktrees/plan3
python3 tools/build.py install && python3 tools/device_test.py
```

Expected: **18/18 automatic PASS** (items 1–7, 9–13 and 15–20; 8 and 14 stay `--manual`). With
both phones plugged in, `systemctl --user set-environment LANTERN_SERIAL=20e9318` first (and
`unset-environment` after). Still deferred:
- Plan 2's remaining deferred checks 2–6 below (manual USB unplug items 8 and 14, Motorola work
  profile, a real PC input method, reboot mid-session, two windows on the real desktop).

**Plan 2 deferred checks, with the commands to run:**
1. ~~Real desktop, unlocked~~ — done, see above.
2. With Sahaj, cable in hand: `python3 tools/device_test.py --manual --only 8 14`.
3. Motorola edge 50 pro (work profile, user 10): with only it attached (or `LANTERN_SERIAL=<its
   serial>`), check the menu has "Files (work)" with the badged icon, open it from the menu
   (`lantern open --user 10 com.google.android.apps.nbu.files`), check the title is
   `Files (work)` and the taskbar icon is badged, and type into its search field.
4. Typing with a real PC input method: start ibus or fcitx5 with a Vietnamese engine, type
   `Tiếng Việt` into the companion's test screen (or any phone app) on the real desktop.
5. Reboot mid-session (see "Still unverified" below): the companion gives the keyboard back
   after the phone restarts with the Lantern keyboard still selected.
6. Two windows on the real desktop: type into the test screen, click a second Lantern window's
   text field, type there, and check nothing typed lands in the other window's field.

### Field findings on the Motorola (2026-11-13, Plan 2 build)

Seen by Sahaj on the installed Plan 2 build with the Motorola edge 50 pro. Not fixed; they are on
the Plan 4 list below.

1. Clipboard is dead while the phone's PIN keyguard is locked (`dumpsys trust` deviceLocked=1):
   apps render behind the keyguard (ALWAYS_UNLOCKED) but Android returns null for every clipboard
   read, so pastes are empty and the helper's listener silently drops phone copies;
   setPrimaryClip still 'succeeds'. Unlocking once with the PIN fixed it (stay-awake keeps it
   unlocked). The Xiaomi has a swipe keyguard (deviceLocked=0). Fix idea: detect deviceLocked at
   session start and notify 'Unlock your phone once for clipboard'; log the null read.
2. default_input_method became Gboard mid-session (journal still recorded set=Lantern IME; cause
   unknown, logcat had rolled). Under IME policy LOCAL, Gboard then popped up inside the PC
   window. Re-selecting the Lantern IME (`ime set dev.lantern.companion/.LanternIme`) fixed it.
   Also enabled_input_methods' journaled orig already contained the Lantern IME (a leftover from
   an earlier session). To investigate.
3. Ctrl shortcuts don't reach the remote Windows session in Windows App
   (com.microsoft.rdc.androidx, work profile), while typing works and Ctrl+A/C/V work in ordinary
   Android text fields. Hypothesis (unconfirmed): injected KeyEvents carry deviceId
   VIRTUAL_KEYBOARD and scanCode 0 (Input.java:146), and RDC uses scan codes for non-text keys.
   Fix idea: fill scanCode from a keycode->evdev table in the helper (no protocol change); confirm
   first with a probe.

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
- **Reboot mid-session** (host-JVM tested only, `ImeWatchdogTest`): with a session live, reboot
  the phone (`adb reboot`) with lanternd stopped or the cable out, unlock it, and check that within
  about 5 s `settings get secure default_input_method` reads the original keyboard again, logcat
  `LanternCompanion` shows `ime created; no helper link: ...` then `ime_restored` and
  `ime_restore done`, and that the next session restores `enabled_input_methods` and stay-awake
  from the journal. Not run because a reboot is disruptive to Sahaj's phone.
- A real password manager's secret hint (tested with a throwaway ext-data-control client, not
  KeePassXC).
- **Encoder crash on resize (observation, not fixed):** in Task 9, 1 of 3 window opens died on the
  first resize to 1024x740 in a nested KWin: `c2.qti.hevc.encoder` "work failed to complete: 14",
  then `video stream failed: fatal codec error: ... Pending dequeue output buffer request
  cancelled`. In Task 10 it did not recur: 0 codec errors in 23 window opens across six suite
  invocations (window logs and logcat checked). `DrainLoop` treats `IllegalStateException` as fatal; worth a look with Plan 1's "failed
  resize" item.
- Plan 3 on a device beyond items 15–20 and the two-phone check: the tray menu opened by hand on
  the Plasma panel, re-registration after a plasmashell restart (host-tested only), Quit and the
  Lantern launcher, start-at-login toggling, Undo phone setup from the tray.
- From Plan 1: PIN keyguard (see field finding 1: the clipboard needs one unlock),
  third-party notification clicks, app-started rotation on a real
  device, long runs with 5–6 encoders, interactive play in games.

## Layout

| Path | What |
|---|---|
| `android/helper/` | Java helper. It runs as the shell user via `app_process` and owns the virtual displays, encoders, input, power, settings, the companion link and the phone clipboard. Built with `source ~/.local/opt/pd-toolchain/env.sh && python3 tools/build.py helper`; host-JVM tests run as part of the build. |
| `android/companion/` | Companion APK `dev.lantern.companion`: `LinkProvider` (the link), `LanternIme` (the invisible keyboard), the launcher catalogue, `LinkReceiver` (test-only `TEST_ACTIVITY`) and the disabled `TestActivity`. Built without Gradle by `python3 tools/build.py companion`. |
| `host/proto/` | Rust crate `lantern-proto`: the wire protocol v1 and golden vectors that both sides test against. |
| `host/lantern/` | Rust crate `lantern`. `lantern daemon` is lanternd: device tracking, helper and companion deploy, session, window IPC, KDE menu catalogue for every phone (`daemon/catalog.rs`), commands and the published view (`daemon/control.rs`), PC clipboard (`clipboard/`), settings file (`settings.rs`), the tray (`tray/`: `menu.rs` pure menu model, `sni.rs`, `dbusmenu.rs`, `mod.rs`), desktop command env (`desktop.rs`). `lantern open <pkg>` is one process per window (winit/wgpu, VA-API decode, text input). `lantern status` prints the daemon state; `lantern connect`/`disconnect` pause and resume the phone. `lantern undo-setup` reverses the phone setup. |
| `docs/protocol.md` | The wire spec, including the companion link. Keep it in step with both sides. |
| `docs/spikes/RESULTS.md` | Risk-spike measurements, plus the Plan 2 IME display policy finding. |
| `tools/build.py` | Targets: `helper`, `companion`, `all`, `install` (also the Lantern launcher `dev.lantern.Lantern.desktop`), `uninstall` (runs `lantern undo-setup` best effort, removes the launcher). |
| `tools/device_test.py` | On-device acceptance suite, items 1–20 (18 automatic): `--manual` adds 8 and 14, `--only N…` (numbers or names), `--nested` (headless KWin for a locked desktop; item 20 still needs the real session bus), `--state`. |

## Environment facts
- Phone: Xiaomi 11T Pro, serial `20e9318`, Android 17 / SDK 37, LineageOS 24, swipe keyguard, USB
  only. Friendly name (`settings get global device_name`) "Xiaomi 11T Pro"; `ro.product.model`
  is `2107113SG`. Its originals: `stay_on_while_plugged_in` 15, keyboard
  `com.android.inputmethod.latin/.LatinIME` (also the whole `enabled_input_methods`).
- Also used: a Motorola edge 50 pro, Android 16, users 0 and 10 (a managed work profile with
  `no_debugging_features`); its `stay_on_while_plugged_in` original is `0`.
- Rust is in `~/.cargo/bin`, which is not on the agent tool's PATH. Prefix commands with
  `export PATH="$HOME/.cargo/bin:$PATH"`. Work in `host/` (`cargo test --workspace --offline`):
  305 tests (1 ignored) on the Plan 3 branch. The helper build runs 170 host-JVM tests, the
  companion build 41. Tests that spawn `lantern daemon` set `DBUS_SESSION_BUS_ADDRESS` to a dead
  socket; tray tests run on a private `dbus-daemon`, never the real session bus.
- Motorola edge 50 pro serial: `ZD222LMDFV`. PIN keyguard (deviceLocked=1 until unlocked once).
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
  journal; the companion's watchdog is the last line for the keyboard, also after a phone reboot
  or battery death mid-session (it persists `ime_prev` in device-protected storage and arms when
  the Lantern IME is created with no link). After such a reboot, stay-awake (7) and the Lantern
  entry in `enabled_input_methods` stay until the next session restores them from the journal.
- Lantern's PC-side files: `~/.config/lantern/settings.toml` (Plan 3), the device records
  `~/.local/state/lantern/devices/<serial>.json` (now with the phone's `name`), the menu entries
  `~/.local/share/applications/lantern.*.desktop` (+ `X-Lantern-Serials=`), icons in
  `~/.local/share/lantern/icons/`, and the launcher `dev.lantern.Lantern.desktop`.
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
  hello reply, so lanternd allows 20 s for the reply (the keyboard-less fallback path takes about
  9 s). lanternd waits for the previous session's helpers before it updates the companion (an
  update kills the companion's process). `ime_prev` (the original keyboard) is a link-only
  message sent after `Session.start`;
  the catalogue starts only on the link-only `sync` sent from the session-up hook, so no bulk
  data waits in the Router's pre-hello buffer.
- **Keyboard.** Journaled before any change, restored in reverse by the helper, by Cleanup on
  helper death, and by the companion's watchdog (5 s after the link is lost or after the IME is
  created with no link, e.g. after a reboot, from a persisted `ime_prev`, with retries and a
  fallback keyboard). Each restore applies only while the setting still holds Lantern's value. If
  the switch fails, the session falls back to stay-awake only and reports
  `companion{unavailable, "keyboard switch failed: …"}`; a session never fails for the keyboard.
- **IME display policy rule.** Lantern displays use `LOCAL` only while the session selected the
  Lantern keyboard, `HIDE` otherwise. Under `HIDE` Android keeps the IME bound to the launcher on
  display 0 and commits never reach the app; under `LOCAL` any keyboard that draws would draw into
  the PC window. `AppDisplay.open` takes it as `lanternIme` (the session's
  `ImeSetup.Started.imeSelected`).
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

## Plan 3 decisions (Sahaj may override)
- **Menu entries for every phone** (Sahaj, 2026-11-10; spec §9a updated). One `.desktop` per
  app_id, `X-Lantern-Serials=a;b;` lists the phones that have it (a Plan 2 file with only
  `X-Lantern-Serial=` reads as that one phone). A matching full sync from phone S only takes S off
  entries it didn't list, per Android user that sent apps; an entry and its icon go when no phone
  lists it; unplugging changes nothing; `undo-setup` takes one phone off. Name/icon from the phone
  that synced last.
- **One session at a time.** `select_serial`: `LANTERN_SERIAL`, else the phone being served if
  still online, else the first online one; when the live phone goes offline, the next online one
  starts. The personal fallback serial is gone. Choosing between two phones plugged in at once is
  Plan 4.
- **Friendly name** = `settings get global device_name`, else `ro.product.marketname`, else
  `<manufacturer> <model>`, stored as `name` in the device record after setup; used by the tray
  and the "Connect your …" notices. Phone names are cached per serial in the run loop and
  refreshed when a session becomes ready and on Online.
- **Open refusal.** After the session's first matching full catalogue, an `open` of an app whose
  entry doesn't list the live phone gets `error{"app not on this phone"}`; the window shows
  "Connect your <name>" (or "Connect A or B", "Connect one of your phones") and exits 1. Before
  that sync nothing is refused. While paused: `error{"phone disconnected in Lantern"}` →
  "Lantern is disconnected".
- **Settings** in `~/.config/lantern/settings.toml` through `SettingsHandle` (a `watch`, polled
  every 2 s): `clipboard_sync` (applies at once; while off lanternd neither sends nor applies a
  `clip`, the helper is not told, Ruling R6) and `lantern_keyboard` (next connection). Missing file
  or key = default (on); wrong type = that key's default + warning; an unparseable file = defaults,
  never overwritten (the tray shows "Settings file has an error; using defaults"). Writes keep
  unknown keys, regenerate the header comment and drop other comments.
- **Keyboard off** rides on `hello.keyboard:false` (omitted when true, v1 bytes unchanged; golden
  `control_hello_keyboard_off.bin`). The helper then runs a stay-awake-only session (no IME
  setting touched, no `ime_prev`, `ime` dropped, IME policy `HIDE`) and reports
  `companion{ready, msg:"keyboard off in Lantern settings"}`.
- **Daemon control.** The run loop takes `Command`s (Connect, Disconnect, UndoSetup, Quit) and
  publishes a `View` (phone state + name) that the tray renders. Disconnect pauses that serial
  (pause set before the session closes; an Online for a paused serial starts nothing) until
  replug or Connect. UndoSetup ends the session, waits for the previous helpers, runs
  `undo::run_steps` inside lanternd and leaves the phone paused. Quit exits 0 (waits for an undo
  in progress); `Restart=on-failure` doesn't restart it. `lantern connect|disconnect` send a
  host-only `daemon{cmd}` and print the `status_report`; `status_report.state` gained `paused`.
- **Tray** written directly on zbus 5: `org.kde.StatusNotifierItem` (icon `smartphone`, overlay
  `view-refresh` / `media-playback-pause` / `emblem-warning`, `Passive` with no phone) +
  `com.canonical.dbusmenu`, registered with `org.kde.StatusNotifierWatcher` and again whenever
  the watcher gets a new owner. The menu is a pure function of `View` + settings (`tray/menu.rs`,
  fixed row ids 1–12). Start at login is systemd's enable state, not a TOML key. Undo asks first
  (`kdialog --warningcontinuecancel`), pins the phone's serial at the click and cancels with an
  "Undo phone setup cancelled" notification if it changed while the dialog was open (Ruling R9).
  No session bus or no watcher: one warning, lanternd runs on.
- **Desktop commands** lanternd spawns (`kdialog`, `xdg-open`, `notify-send`) get
  `WAYLAND_DISPLAY`/`DISPLAY`/`XDG_CURRENT_DESKTOP` from `systemctl --user show-environment` when
  missing (`desktop::session_env`, shared with the clipboard).
- **`build.py install`** adds the Lantern launcher (`dev.lantern.Lantern.desktop`, runs
  `systemctl --user start lanternd.service`, no `X-Lantern-Serial`, so the catalogue never
  touches it); `uninstall` removes it. `undo-setup`'s refusal now also points at the tray.
- **Cargo.lock** offline-resolution downgrades (clap, wasm-bindgen, js-sys, toml_edit, winnow)
  were accepted for `zbus`/`toml` (Ruling R7); re-resolve when online.

## Next: Plan 4 (not yet written)

Plan 3 is merged. Next is Plan 4a, unlocking a PIN-locked phone from the PC (design:
`docs/superpowers/specs/2026-11-13-lantern-unlock-prompt-design.md`, spike done). Then write Plan 4 with superpowers:writing-plans and run it with
superpowers:subagent-driven-development. Candidates (Plan 3's "Out of scope" plus the field
findings):

- Notifications from the phone in KDE, with clicks opening that app's window (§8.5).
- Drag and drop from PC to phone: copy the file, then use Android share, falling back to Downloads.
- One phone audio stream.
- Gamepads: the helper must re-attach the UHID controller to the display of the active PC window.
  Spikes showed scrcpy keeps it pinned to the first display. On Linux the GameSir must be held
  open, or it reconnects over and over.
- Pausing a window's encoder while it is minimised, within protocol v1 (see `docs/protocol.md`).
- A latency harness, and the target from spec §9a/§11: no more than 5 ms worse than scrcpy.
- Choosing between two phones plugged in at the same time (today the first one online wins).
- A settings window (the tray holds the few settings today).
- Field finding 1: clipboard while the PIN keyguard is locked — detect `deviceLocked` at session
  start, notify "Unlock your phone once for clipboard", log the null clipboard read.
- Field finding 2: `default_input_method` changed to Gboard mid-session — find the cause; consider
  the helper re-asserting the Lantern keyboard or reporting a keyboard changed by someone else
  (and a stale Lantern entry already in the journaled `enabled_input_methods` original).
- Field finding 3: Ctrl shortcuts in Windows App's remote session — probe first, then fill
  `scanCode` from a keycode→evdev table in the helper (no protocol change).

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
    hello reply takes about 9 s of lanternd's 20 s; the stale-armed watchdog if the hello write
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
  - Companion install: `ensure()` has no overall timeout (worst case about 7 min before a session
    starts); a cancel leaves the pushed APK; retries re-push; the notice says "Installed" for an
    already-current companion with no record; `pm uninstall --user 0` leaves other users' copies;
    `adb devices` in undo-setup has no timeout; the "no daemon" message order in undo-setup.
  - Catalogue: file IO and base64 decoding run on the async loop; an odd serial that doesn't
    round-trip through `escape_value` would self-prune; the host doesn't cap icons at 1 MiB; a
    failed `kbuildsycoca6` spawn is silent; undo-setup rewrites the menu files; a deleted
    profile's entries linger until undo-setup.
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
    recreated per display, and an `ime` for another display than the IME's is dropped on purpose
    so text never lands in the previous window's editor); a dead key with Ctrl/Alt/Super is swallowed; CapsLock/NumLock
    characters go as text, not keys; the `LANTERN_DEBUG_INPUT` step log prints the typed test text.
  - "Connect your phone" also shows when lanternd isn't running at all.
- **Plan 3** (`minor (deferred)` in the Plan 3 SDD ledger; none leaves the phone changed)
  - Catalogue (multi-phone):
    - A serial with control characters or over 200 chars can't take itself off an entry
      (`escape_value` drops/truncates); real adb serials are unaffected.
    - undo-setup's "removed {n} entries" also counts shared entries kept for another phone
      (`undo.rs`).
    - `upsert` writes the menu files before the entry, so a failed entry write can leave an empty
      Phone menu.
    - An empty `X-Lantern-Serials=` value makes an entry nobody can remove; fall back to
      `[X-Lantern-Serial]` when it is empty.
  - Phone names and "Connect your …":
    - `read_phone_name` keeps trying after an adb error (worst case about 40 s added to the
      session start); stop on the first error (`companion.rs`).
    - Phone names go unescaped into the notify-send body (Plasma markup: `&`, `<`, `>`).
    - The device record's name temp file isn't fsynced before the rename.
    - The window's "here" phone can be wrong if the session changes between the refusal and its
      status query.
  - Settings:
    - A broken or half-written settings.toml gives the defaults (sync on) until it parses again.
    - `SettingsHandle::set` does blocking file IO on the async runtime.
    - A tray toggle reads then sets outside the settings lock, so two quick clicks can lose a
      flip (wants a `SettingsHandle::toggle`).
  - Keyboard setting (helper):
    - `keyboard:null` or an object in `hello` decodes as absent, i.e. true (only lanternd sends
      it; true is the journaled default path; `Router.java`).
    - `ImeSetup` shares one "drop logged" flag across drop reasons (one extra or missing log
      line).
  - Daemon control:
    - The pause is one slot: disconnect A, B starts, disconnect B unpauses A; unplugging B then
      starts A without a Connect. The final review must call this MUST-FIX or OK-TO-DEFER.
    - The undo future runs inline in the run loop, and `remove_entries`' `block_in_place` stalls
      the loop briefly.
    - Quit during an undo stops serving clients until the undo ends.
    - An Offline of the phone during its undo clears the pause (status `idle` while the view says
      Undoing).
    - After an undo with the phone unplugged and another phone online, the other phone waits for
      the next device event.
    - A first-online phone stuck in start backoff keeps a second phone from starting.
  - Tray:
    - Repeated Undo clicks open several kdialogs (the daemon refuses the extra ones while
      undoing).
    - Concurrent Start-at-login enable/disable clicks race (cosmetic).
    - Registration is awaited inline in the tray's select loop (delays a menu refresh up to 5 s).
    - A millisecond race remains between the tray's re-read of the phone and the daemon's
      UndoSetup target pick (Ruling R9 accepted).
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
