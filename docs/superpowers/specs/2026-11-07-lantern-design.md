# Lantern — design

Date: 2026-11-07 · Status: approved by Sahaj; actual test phone updated to Android 17 / LineageOS 24 during preflight · Owner: Sahaj

## 1. Intent

Run Android apps from a phone as **native-feeling windows on a Linux (KDE Plasma) PC**. Each Android
app is its own PC window: its own KDE menu entry, taskbar entry, icon, Alt-Tab entry, resizable like any
other window. The phone sits on the desk, screen off, connected by USB.

Lantern replaces the approach of `~/repos/phone-desk` (PhoneDesk), which drew a DeX-style desktop
(taskbar, start menu, freeform windows) inside a single scrcpy window. Sahaj judged that architecture
not clean or robust enough. Lantern is a fresh codebase: it reuses PhoneDesk's *knowledge*
(see §10), not its code structure.

The bar is **robust and polished**: it should behave like an installed PC app, not like a hack
that sometimes works.

## 2. Goals (v1)

1. **One PC window per Android app.** KDE treats each as a separate application: correct name, icon,
   taskbar grouping, Alt-Tab, window rules.
2. **Launch from the KDE app menu.** Every launchable phone app (including work-profile apps) gets a
   menu entry with its real icon; entries track installs/uninstalls on the phone.
3. **Resizing works natively.** Resizing the PC window resizes the app's Android display; the app
   relays out (no letterboxing/scaling as the steady state).
4. **Close = quit.** Closing the PC window quits the Android app (PC semantics, chosen by Sahaj).
   Launching an app that's already open focuses its existing window (one window per app).
5. **Notifications** from the phone appear as KDE notifications (app name + icon). Clicking one opens
   that app in its window, ideally at the notification's target (the specific chat), not just the app.
6. **Clipboard** syncs both ways (text in v1).
7. **Drag-and-drop files** onto a window delivers them to that app (Android share). If the app can't
   accept them, the file is saved to the phone's Downloads and a KDE notification says so.
8. **PC keyboard** types into apps, including IME-composed text (e.g. Vietnamese via the PC's input
   method). The Android on-screen keyboard never appears.
9. **Audio** from the phone plays on the PC (one mixed stream). The phone's speaker stays silent.
10. **Gamepads** connected to the PC appear on the phone as real external controllers (Xbox layout,
    one per pad) and drive whichever Lantern window is focused.
11. **Low latency** good enough for games with native controller support: hardware decode on the PC,
    present frames immediately, encoder low-latency mode, up to the phone's max refresh where possible.
12. **Zero-ceremony sessions.** Plugging the phone in starts everything in the background; unplugging
    ends the session cleanly.
13. **Works on the test phone with only USB debugging enabled.** No freeform/desktop developer flags, no reboot,
    **no root**.

## 3. Non-goals (v1)

- Support for anyone else's hardware: Android versions below 16, other phones/OEMs, non-Intel GPUs,
  non-KDE desktops, public release packaging.
- Wi-Fi / Tailscale connections (planned later; see §6.1 — transport must be swappable).
- Windows / macOS hosts.
- A desktop/taskbar on the phone side (no DeX recreation).
- Using the phone in-hand during a session (it must not crash or corrupt state, but no handover rules).
- Multiple windows of the same app.
- Per-app volume; image clipboard; notification reply/actions.
- PC microphone or webcam feeding Android apps (needs root; future optional add-on). Apps use the
  phone's own mic/camera.
- Touch-to-controller mapping for touch-only games.
- Root-only features (protected/FLAG_SECURE screens, mic injection, clock locking) — future optional
  "root booster", must never be required.

## 4. Anti-goals (would make it a failure even if it "works")

- Leaving the phone in a changed state after a session (screen off, wrong keyboard, stay-awake on,
  apps stranded) — especially after an unclean unplug or crash.
- Quietly changing global phone settings without telling the user.
- All windows showing as one generic app in the KDE taskbar.
- One crashing app window taking down the others.
- Requiring root, freeform flags, or a reboot.

## 5. Constraints and environment

- **PC:** Linux, KDE Plasma 6.6 on Wayland (KWin). Intel UHD (CometLake GT2) → VA-API H.264/HEVC decode.
  Rust toolchain present at `~/.cargo/bin` (not on PATH by default). scrcpy 5.0.1 and adb at
  `~/.local/bin`.
- **Phone (test device):** Xiaomi 11T Pro, LineageOS 23 (Android 16 / SDK 36), adb serial `20e9318`.
  Sahaj will reset developer settings so only USB debugging is on.
- **Audience (Sahaj's decision): v1 is for Sahaj only** — this Xiaomi (Android 16) and this Intel/KDE PC.
  Target Android 16 only; no code paths or testing for older Android versions, other OEMs, non-Intel GPUs
  or other desktops. Keep hidden-API calls behind the vendored wrapper layer so widening later is
  possible, but don't build for it. Packaging = a local install script, not a distributable release.
- **Android toolchain:** `~/.local/opt/pd-toolchain` (JDK 21, SDK 36 platform, build-tools, jadx). PhoneDesk
  built its APK without Gradle via `tools/build.py`; same approach is fine.
- **Licensing:** code copied from scrcpy (Apache-2.0) keeps its headers and attribution; vendored files
  record the upstream version.
- **No root**, no Shizuku. All privileged work is done by a helper running as the ADB `shell` user.

## 6. Approaches considered

### 6.1 Overall architecture

| Option | Summary | Verdict |
|---|---|---|
| **Wrap scrcpy** (one `scrcpy --new-display --start-app` per app + side daemon) | Fastest to a demo | Rejected: N processes/connections, windows are scrcpy's (wrong app identity, IME, close, DnD) → back to hacks |
| **One hidden big display, crop each app** (Phone Link-like) | One encoder | Rejected: overlapping apps occlude each other on the phone → wrong pixels |
| **Own system: phone helper with one virtual display per app + PC client** | Most work, cleanest | **Chosen** |

One virtual display per app means each app is simply fullscreen on its own display. No freeform
windowing, no window management on the phone at all — KWin does window management.

### 6.2 PC-side structure

KDE identifies a window's app (icon, name, grouping) by the Wayland `app_id` matched to a `.desktop`
file. Toolkits like SDL set one app_id per process.

| Option | Summary | Verdict |
|---|---|---|
| **A. Daemon + one window process per app** | `lanternd` owns the phone connection; `lantern open <pkg>` spawns a small window process with app_id for that app | **Chosen**: correct KDE identity for free, crash isolation |
| B. Single process, draws all windows, sets app_id per toplevel directly | Fewer processes | Rejected: one crash kills all windows; still needs IPC for menu launches |

### 6.3 scrcpy: fork vs borrow

Not a fork. Lantern writes its own phone helper and **vendors selected scrcpy server code**: the
hidden-API wrappers (display, input, clipboard, power), encoder setup and per-device encoder workarounds,
audio capture, UHID controller support. Vendored code lives in its own directory with an
`UPSTREAM` note (scrcpy 5.0.1) so upstream fixes can be diffed and pulled. Rationale: scrcpy's server
is one-display/one-stream; Lantern needs multi-display multiplexing, per-app lifecycle and focus routing.

### 6.4 Root

No root for now. Root would not reduce stream latency, and it breaks many games with anti-cheat. It may
come later as an optional booster.

## 7. Architecture

```
 PC (KDE Wayland)                                         Phone (Android 11+, USB)
 ┌──────────────────────────────┐                        ┌───────────────────────────────────┐
 │ lanternd (systemd user svc)   │  one multiplexed      │ lantern-helper (app_process,      │
 │  - device watch (adb)         │  stream over          │   shell uid)                      │
 │  - session setup/teardown     │  adb forward ───────► │  - virtual display per app        │
 │  - mux/demux channels         │                        │  - encoder per display            │
 │  - .desktop + icon writer     │                        │  - input / key / text injection   │
 │  - notifications (D-Bus)      │                        │  - audio capture                  │
 │  - clipboard (data-control)   │                        │  - clipboard, UHID gamepads       │
 │  - gamepad capture            │                        │  - screen power, cleanup          │
 │  - unix socket for windows    │                        │       ▲ token-protected local     │
 └──────────▲───────────────────┘                        │       │ socket                     │
            │ $XDG_RUNTIME_DIR socket                     │ Lantern companion app (APK)       │
 ┌──────────┴───────────┐  ┌──────────────────────┐      │  - app list + icons (LauncherApps)│
 │ lantern window       │  │ lantern window       │ ...  │  - notification listener          │
 │ app_id=…whatsapp     │  │ app_id=…chrome       │      │  - invisible input method (IME)   │
 │ decode (VA-API) +    │  │                      │      │  - file receiver (FileProvider)   │
 │ render, input, DnD,  │  │                      │      └───────────────────────────────────┘
 │ text-input-v3        │  │                      │
 └──────────────────────┘  └──────────────────────┘
```

### 7.1 Components

**PC: `lantern` (Rust, one binary, subcommands)**
- `lantern daemon` (`lanternd`): started at login as a systemd user service. Watches adb for the
  phone, runs session setup, owns the single phone connection, multiplexes channels, serves window
  processes over a Unix socket in `$XDG_RUNTIME_DIR` (mode 0700). Writes `.desktop` files and icons.
  Bridges notifications (freedesktop D-Bus), clipboard (Wayland data-control, which KWin supports), and
  gamepads (SDL or evdev).
- `lantern open <package>[@user]`: the `Exec` of each `.desktop` entry. If that app's window already
  exists, focus it (via the daemon / XDG activation token). Otherwise become the window process: open
  a Wayland toplevel with app_id matching the `.desktop` file, ask the daemon for a display for the
  package, receive that display's encoded video, decode (VA-API, software fallback), present, and send
  input back.
- Encoded video (H.264/HEVC packets) is forwarded daemon → window process; decoding happens in the
  window process.

**Phone: `lantern-helper` (Java, runs via `app_process` as shell, like scrcpy-server)**
- Creates/resizes/destroys one virtual display per open app; launches the app on it; reports when the
  display's last task is gone.
- One hardware encoder per display; reconfigures on resize.
- Injects touch/mouse/scroll/key events targeted at a display ID; moves Android focus to the display of
  the focused PC window (needed for keyboard and gamepads).
- Captures audio (silent on the phone speaker), clipboard get/set/listen, UHID gamepads (with
  PhoneDesk's USB-bus patch so games see external controllers).
- Turns the physical panel off (scrcpy-style display power mode, phone stays awake and unlocked) and
  restores it.
- Removes tasks (quit) on window close.
- Owns teardown (§8.4): it must survive the USB link dying long enough to clean up.

**Phone: Lantern companion app (APK, package e.g. `dev.lantern`)**
- Enumerates launcher apps incl. work profile (LauncherApps), renders icons to PNG, reports
  install/uninstall changes.
- NotificationListenerService: forwards posted/removed notifications (key, app, title, text, icon,
  timestamp); keeps the PendingIntents for click handling.
- An invisible InputMethodService so committed/composed Unicode text from the PC IME reaches apps as
  real text input.
- FileProvider + receiver for drag-and-drop deliveries.
- Talks to the helper over a token-protected local socket (abstract sockets are reachable by any app
  on the phone, so the token is mandatory). It does not talk to the PC directly.

### 7.2 Transport and protocol

- v1 transport: `adb forward` to the helper's local socket. One stream, length-prefixed frames with a
  channel ID (control, per-display video, audio, clipboard, companion messages, gamepad).
- The transport sits behind one interface so Wi-Fi/Tailscale (own TCP + pairing + encryption + adaptive
  bitrate) can be added later without touching the rest.
- Version handshake on connect. The daemon pushes the helper jar and installs/updates the companion APK
  on version mismatch.

## 8. Key flows

### 8.1 First-time setup (consented, reversible)
Run on first connection of a new phone; the daemon shows a KDE notification or small dialog explaining
each step. Steps:
1. Install companion APK.
2. Grant via shell: notification-listener access, `WRITE_SECURE_SETTINGS` to the companion (needed for
   self-restoring the keyboard, §8.4), enable the Lantern input method.
3. Record the original value of every setting Lantern will touch, per device, on the PC.
`lantern undo-setup` reverts all of it (and uninstalls the companion if asked).

### 8.2 Session start (on plug-in)
Daemon sees the device → push/verify helper and companion → start helper → connect → turn the phone
panel off, keep awake → select Lantern IME for the session → sync app list/icons into
`~/.local/share/applications` (and icons) → ready. Windows open on demand.

### 8.3 App window lifecycle
- **Open:** `lantern open pkg` → daemon asks helper for a display sized to the window (remembered per
  app, else a sensible default; portrait-only apps get a portrait window) → helper creates display +
  encoder, launches the app on it → video flows.
- **Resize:** window process tells daemon the new size (debounced during drag; scaled old frames shown
  meanwhile) → helper resizes the virtual display and reconfigures the encoder. PC scale factor maps to
  display density.
- **Focus:** PC focus → helper moves Android focus to that display.
- **Close (X):** helper removes the app's task(s), destroys the display and encoder.
- **App exits by itself** (last task on its display gone): the PC window closes.
- **Launching an already-open app:** focuses the existing window.

### 8.4 Session end and recovery
- **Clean unplug or daemon stop:** remove Lantern-hosted tasks (quit), destroy displays, restore panel
  power, stay-awake and the previous IME.
- **Unclean (USB yanked, helper killed):** the helper detects link loss and runs the same teardown,
  then exits. Belt and braces: the companion app also watches for a dead helper and restores the
  previous IME itself, so the phone is never left with an invisible keyboard.
- **Helper crash mid-session (USB still present):** the daemon restarts the helper. Windows show a
  "reconnecting" state, then are re-established best-effort.
- **Menu entries while disconnected** stay in the KDE menu; launching one shows a "Connect your phone"
  notification. (This is a builder call; KDE can't grey out entries.)

### 8.5 Notifications
Companion forwards a notification → daemon posts a KDE notification with the app's name and icon →
on click, the daemon opens/focuses that app's window, then asks the phone to fire the notification's
content intent onto that app's display. If firing on that display is refused, fall back to launching
the app's main activity on its display. Dismissal is synced both ways (builder call).

### 8.6 Clipboard, drag-and-drop, keyboard, audio, gamepads
- **Clipboard:** text, both directions, with loop suppression.
- **DnD:** the window accepts a file drop → file is copied to the phone (companion's storage) → the
  helper launches `ACTION_SEND`(`_MULTIPLE`) targeted at that package on its display with a
  content URI grant. If the package has no matching handler, save to Downloads + KDE notification.
- **Keyboard:** shortcuts, navigation and non-text keys are injected as key events to the focused
  display. Text, including composed text from the PC input method (Wayland text-input-v3), goes
  through the Lantern IME as commit/compose text. Android's IME display policy is set so no on-screen
  keyboard is shown on Lantern displays.
- **Audio:** one stream, low-latency playback through PipeWire.
- **Gamepads:** captured on the PC, one UHID device per pad on the phone. Input goes to the focused
  Lantern window's display.

## 9. Risks — validate in early spikes, before building on them

1. **Encoder concurrency:** how many simultaneous hardware encoder sessions the Xiaomi sustains at
   useful resolution. Light multitasking only, but a minimum of 3–4 is needed.
2. **Locked phone:** whether a shell-created virtual display can use the "always unlocked" flag so
   apps show even when the phone was locked at plug-in. Fallback: "Unlock your phone to start"
   notification.
3. **Notification click on a specific display:** whether a PendingIntent can be fired onto a Lantern
   display.
4. **Focus routing:** moving Android's focused display reliably so keys and gamepads reach the right app.
5. **IME:** invisible IME + display IME policy giving reliable Unicode/composed input across apps;
   self-restore works after a yank.
6. **Resize:** virtual display resize + encoder reconfigure is fast and glitch-free enough during
   window drags.
7. **App compatibility:** some apps may refuse or misbehave on secondary displays. FLAG_SECURE content
   renders black (accepted without root). Catalogue these; don't chase them in v1.
8. **Activities launched across apps** (e.g. an app opens the browser) land on the caller's display, so
   they appear inside the caller's window. Accepted for v1 (builder call); a later version may give
   them their own window.

**Spike order (my call):** spikes 1–6 run first as throwaway probes, before the real helper is built;
the build plan starts only after their results are written up.

**If encoder concurrency is low (my call):** degrade in this order. (a) Pause the encoder for windows
KWin reports as minimised/suspended, resuming with a keyframe. (b) Lower resolution/fps for unfocused
windows. (c) Cap open windows: refuse a new one with a KDE notification naming the limit. Software
encoding is not used. Pause/resume is part of the v1 protocol regardless, because it saves battery and
heat.

## 9a. Decisions added after builder review (my calls; Sahaj may override)

- **Orientation changes mid-session:** the app wins. When the app requests a different orientation, the
  helper reports it and the window process asks KWin to swap the window's aspect (keep the long edge's
  size). If the compositor refuses (e.g. maximised or tiled), letterbox inside the window. Letterboxing is
  accepted only in that case.
- **One phone at a time.** Naming and session state assume a single active device. Connecting a different
  phone replaces the menu entries (the previous phone's entries are removed). Settings records are kept per
  device serial so undo-setup still works for each.
- **Status tray:** a small KDE status-tray item (StatusNotifierItem) from `lanternd` shows connected/
  disconnected/reconnecting and offers Disconnect, Open settings folder, Undo phone setup and Quit. This
  makes per-session phone changes visible and reachable without the CLI.
- **Latency target:** glass-to-glass no more than 5 ms worse than scrcpy 5.0.1 on the same phone, cable and
  settings, measured by the §11 harness. Start with a plain socket relay daemon → window. If it misses the
  target, switch to handing the window process its channel directly (fd passing) — keep the relay behind an
  interface so this is a local change.

## 10. Carry over from PhoneDesk (knowledge, not structure)

- The shell-UID `app_process` helper pattern and token-protected local socket.
- Gamepad: patched scrcpy-server UHID bus constant so games treat pads as external (fixes Diablo
  Immortal-style rejection). See PhoneDesk `README.md` § Gamepads and `tools/build.py`.
- Lessons: reconcile from read-back Android state, never assume your own commands are the only
  mutators. Record and restore every setting touched, including "key was absent" (never write a literal
  `null`). Never change global settings or reboot without consent.
- Device notes: `docs/RE_FINDINGS.md`, `docs/XIAOMI_TEST.md`, `docs/XIAOMI_FREEFORM_RETEST.md`
  (freeform findings are mostly moot now, since Lantern doesn't use freeform).

## 11. Testing

- Unit tests for protocol framing/mux, settings record/restore, `.desktop` generation, and the
  window-lifecycle state machine (Rust and Java).
- Device integration runs on the Xiaomi 11T Pro: open/resize/close, yank-USB teardown leaves the phone
  clean (panel on, IME restored, stay-awake restored, no stranded tasks), notification click, clipboard
  round-trip, DnD, gamepad.
- Latency measurement harness (e.g. on-screen timestamp app vs PC capture) to compare against scrcpy.

## 12. Left to the builder

- Rust crates and toolkit for the window process (must set per-window app_id and support
  text-input-v3, DnD and XDG activation), decoder library, D-Bus/PipeWire libraries.
- Exact protocol encoding; channel and flow-control details.
- `.desktop`/app_id naming scheme (must be valid, unique per package + profile user, and match).
- Default window size, debounce timings, bitrate/codec defaults (prefer HEVC if supported, else H.264).
- Project layout, build scripts (PhoneDesk-style `build.py` or Gradle), packaging and install script.
- Calls marked "builder call" above.

Repo: `~/repos/lantern` (GitHub `SahajJain01/lantern` when published). License: Apache-2.0 (my call;
matches vendored scrcpy code).
