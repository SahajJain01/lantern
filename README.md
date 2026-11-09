# Lantern

Run Android apps from a phone plugged into your PC as ordinary KDE Plasma (Wayland) windows.
Each app gets its own window, taskbar entry, Alt-Tab entry and remembered size. Each app also runs
on its own virtual display on the phone, so Lantern doesn't mirror the phone's screen and the apps
don't share one canvas. Close the window and the app quits on the phone.

> **Status: pre-release (v0.1.0-alpha).** Lantern is a personal project. It is built and tested on
> one setup: KDE Plasma 6 Wayland on Ubuntu 26.04 with an Intel GPU, plus a Xiaomi 11T Pro on
> Android 17 (LineageOS 24) and a Motorola edge 50 pro on Android 16, over USB. Other phones,
> Android versions, GPUs and desktops may not work. Text typing, clipboard, audio and notifications
> are not here yet; see the [Roadmap](#roadmap).

## How it works

Two parts, both in the one `lantern` binary:

- `lanternd` (`lantern daemon`, a systemd user service) watches adb for the phone and deploys a
  small helper to it. It keeps one session open while the phone is plugged in.
- `lantern open <package>` is one window. It decodes that app's H.265/H.264 stream with VA-API and
  draws it with wgpu. Mouse, scroll and keys go back to the app.

The phone side is a Java helper started over adb as the `shell` user (`app_process`, no root and
no installed app). It creates one virtual display and hardware encoder per window, and injects
input. It borrows a few wrappers from [scrcpy](https://github.com/Genymobile/scrcpy) 5.0.1
(Apache-2.0, vendored under `android/helper/vendor`). The wire protocol is in
[`docs/protocol.md`](docs/protocol.md).

## Requirements

- KDE Plasma on Wayland. VA-API is used for hardware decoding; software decoding is the fallback.
- **Runtime libraries:** FFmpeg (`libavcodec`, `libavutil`) and `libva`.
- **To build:**
  - the Rust toolchain;
  - the FFmpeg and libva development packages, plus clang and pkg-config;
  - a JDK and the Android build tools for the helper.
- **Helper toolchain:** `tools/build.py` expects the JDK, Android `build-tools` (`d8`) and
  `platform/android.jar` (API 35+) under `~/.local/opt/pd-toolchain`. Override that path with
  `PD_TC`.
- `adb` on `PATH`, with the phone authorized for USB debugging. The phone must run Android 15 or
  newer (API 35).
- lanternd serves the only connected device. With several devices connected, set
  `LANTERN_SERIAL` to the one it should serve: run `systemctl --user edit lanternd` and add
  `[Service]` / `Environment=LANTERN_SERIAL=<serial>`. `adb devices` lists the serials.

lanternd runs `adb` from the systemd user manager's `PATH`, not your shell's. If `adb` lives
somewhere else (for example `~/Android/Sdk/platform-tools`), add it to that `PATH` in one of two
ways:
- in `~/.config/environment.d/*.conf`: `PATH=$HOME/Android/Sdk/platform-tools:$PATH`. This applies
  from the next login.
- with `systemctl --user edit lanternd`:
  `Environment=PATH=<dir with adb>:/usr/local/bin:/usr/bin:/bin`.

Check the result with `systemctl --user show-environment | grep ^PATH`.

### Prebuilt binary

The [releases](https://github.com/SahajJain01/lantern/releases) page has a prebuilt x86_64
`lantern` with the helper embedded. It is built on Ubuntu 26.04 and links FFmpeg 8
(`libavcodec.so.62`). To install it:

```sh
tar xf lantern-*-x86_64-linux.tar.gz && cd lantern-*/
install -Dm755 lantern ~/.local/bin/lantern
install -Dm644 lanternd.service ~/.config/systemd/user/lanternd.service
systemctl --user daemon-reload && systemctl --user enable --now lanternd
```

## Build and install

```sh
export PD_TC=~/.local/opt/pd-toolchain   # jdk/, build-tools/, platform/android.jar
python3 tools/build.py install
```

`install` builds the phone helper jar (running its host-JVM tests), embeds it in a release
`lantern`, copies that to `~/.local/bin/lantern`, and installs
`~/.config/systemd/user/lanternd.service`. It then runs `systemctl --user daemon-reload` and
`systemctl --user enable --now lanternd`. Running it again is safe: it restarts a running
daemon only when the binary or the unit changed.

Build only: `python3 tools/build.py helper` (jar) or `python3 tools/build.py all` (jar + `cargo build --release`).
Tests: `cd host && cargo test`.

## Use

```sh
lantern open com.android.settings     # one window per app; opening it again focuses that window
lantern open --user 10 com.android.settings   # the same app from Android user/profile 10 (e.g. work)
lantern status                        # daemon state as JSON
```

`--user N` (default `0`) picks the Android user or profile the app runs as; `adb shell pm list
users` lists them. Each user's copy of an app is its own window: Wayland app_id
`lantern.<pkg>` for user 0 and `lantern.<pkg>-u<N>` otherwise, title `<pkg> (user N)`, and its own
remembered size. `<pkg>` must be a package name (letters, digits, `_`, `.`), and `N` a plain
decimal number; anything else is a usage error (exit 2).

Work profiles whose admin bars adb (Intune and most MDMs set `no_debugging_features`, so
`am start --user 10` says "Shell does not have permission to access user 10") still open: for a
profile of the current user (work profile, private space, clone) Lantern lists and starts the app
through Android's LauncherApps service, the same cross-profile path the phone's own launcher uses.
Users that are not profiles of the current user and also bar adb cannot be opened. The phone
refuses (and the window prints why) when the user does not exist, is not running or not unlocked
(a paused work profile), or does not have the app. A profile app that already has a task on the
phone (on its screen or in recents) may be moved onto its Lantern window (LauncherApps cannot ask
for a new task); the window then prints `lantern: <pkg> (user N) was already open on the phone; it
moved into this window and closing the window will quit it`.

`lantern status` prints `{"state":"idle|starting|ready","serial":…,"helper":<jar sha256>,"windows":[{"id","pkg","user"}]}`.
Logs: `journalctl --user -u lanternd`. Set `LANTERN_LOG=debug` for more detail.

Exit statuses: 0 ok, 1 runtime failure, 2 usage error, 3 helper protocol mismatch (`daemon`
only; systemd does not restart it).

## What it does to your phone

Whenever the phone is plugged in and lanternd is running (the service starts with your login
session), lanternd starts a session on the phone at once, even if no window is open:

- **The phone's own screen (panel) is off while a session runs.** The phone stays awake, and
  the apps keep drawing to their Lantern displays. The panel comes back on when the session ends.
- **The "stay awake while charging" setting is changed and later restored.** The setting is
  `settings global stay_on_while_plugged_in`; it is set to `7` during the session. The original
  value is restored when the session ends, including when the helper is killed or the cable
  is pulled. A separate guard process on the phone does that restore. The original value is also
  journaled to `/data/local/tmp/lantern-session.json` before anything changes, and the file is
  deleted after a clean restore. If both helper and guard die without restoring (reboot, battery
  death, `kill -9` of both), the next session takes the original from that file rather than the
  current value, as long as the setting still holds the value Lantern wrote, and restores it when
  it ends. A setting you changed yourself in the meantime is never overwritten. A new session
  also waits (up to 10 s) for the previous session's helper processes to exit, killing any still
  alive after that, so two sessions never overlap on the phone.
- **One virtual display per open window**, named `lantern-<id>-<package>`. Each is removed when its
  window closes, and closing a window quits its app's task (close = quit).
- **The helper jar stays in `/data/local/tmp`** as `lantern-helper-<sha256>.jar`, and the helper
  runs as the `shell` user (`app_process`) only during a session. Every session start deletes
  the jars of other Lantern builds (`lantern-helper-*.jar` other than the current one). lanternd
  also adds one `adb forward` for the session and removes it when the session ends.
- **No keyboard is installed or switched yet.** The hidden Lantern keyboard (input method) arrives
  only in Plan 2. Until then, the on-screen keyboard is just hidden on Lantern displays, and those
  displays are removed with their windows.

The phone is restored whenever the session ends: when you stop lanternd
(`systemctl --user stop lanternd`), unplug the phone, the daemon dies, or you log out. Restored
means the panel is on, the stay-awake setting has its old value, there are no Lantern displays
or helper processes, and only the current jar file remains on the phone (plus
`lantern-session.json` while a session is live, or after one that nothing could restore). A
daemon that stops cleanly also removes its `adb forward`. One that is killed outright
(`SIGKILL`, a crash) cannot, so its forward stays in the PC's adb server until the next session
for that phone starts: every session start first removes that phone's leftover
`localabstract:lantern-*` forwards. To drop one by hand, `adb forward --list` shows it and
`adb forward --remove tcp:<port>` removes it.

## Undo / uninstall

```sh
systemctl --user stop lanternd        # pause: ends the session and restores the phone now
systemctl --user disable --now lanternd   # stop starting it at login

lantern undo-setup [--serial <serial>]   # with lanternd stopped: undo what Lantern did to the phone
python3 tools/build.py uninstall      # disable --now, undo-setup (best effort), remove the unit
                                      # and ~/.local/bin/lantern
```

lanternd installs the Lantern companion app (`dev.lantern.companion`: keyboard and app list) on the
phone, and keeps it at the build inside `lantern`. The first time it sets up a phone it shows a
notification "Lantern set up your phone" and records the phone in
`~/.local/state/lantern/devices/<serial>.json`. If the install fails, the session runs without the
companion and one notification per daemon run says so (details in `journalctl --user -u lanternd`).

`lantern undo-setup` refuses whenever lanternd answers on its socket, even when idle (it may be
about to retry a session). It cannot see a lanternd on another runtime dir or another program using
adb on the phone; stop those first. It restores any
setting a dead session left changed (only where the setting still holds Lantern's value), deletes
Lantern's files from `/data/local/tmp`, uninstalls the companion, and forgets the phone. It prints one
line per step and exits 0 only if every step succeeded. Without `--serial` it uses `$LANTERN_SERIAL`
or the only connected phone.

`uninstall` is idempotent. It runs `lantern undo-setup` after stopping the daemon; if the phone is not
connected then, run it again later (`lantern undo-setup` is gone once `lantern` is removed, so
reinstall or remove the companion with `adb uninstall dev.lantern.companion`).

## Acceptance test (real phone + KDE desktop)

```sh
python3 tools/build.py install && python3 tools/device_test.py           # items 1–7, unattended
python3 tools/device_test.py --manual                                    # adds 8: USB unplug/replug, prompts
python3 tools/device_test.py --signal=KILL                               # item 7 with SIGKILL
python3 tools/device_test.py --state                                     # status + phone readbacks
```

The test restarts lanternd. Item 1 stops it first and reads the phone's own
`stay_on_while_plugged_in` value, which later items expect to see restored; set
`LANTERN_STAY_ON_RESTORED=<value>` to give it instead. `--signal=KILL` checks that systemd
restarts a SIGKILLed lanternd and that the new session removes the dead daemon's forward.
The test opens only `com.android.settings` and `org.lineageos.jelly`, briefly, and drives
windows through the KWin scripting D-Bus.

## Layout

- `android/helper/`: the phone helper (Java, no Gradle; vendored scrcpy 5.0.1 wrappers).
- `host/proto/`: wire protocol (`docs/protocol.md`).
- `host/lantern/`: daemon and window (Rust).
- `tools/`: `build.py` (build, install), `device_test.py` (acceptance).

## Roadmap

Plan 1 (core) is done: per-app windows, video, mouse/scroll/keys, resize with relayout, close = quit,
the phone restoring itself, work profiles, install, and an on-device acceptance suite. Next:

**Plan 2: feels like a desktop app**
- KDE launcher entries and icons that follow the phone's installed apps.
- Window titles show the app's name instead of its package name (`com.foo.bar (user 10)`), with
  "(work)" for managed profiles.
- Typing text through a hidden Lantern keyboard (input method), including composed input such as
  Vietnamese.
- Text clipboard in both directions.
- Phone-safety hardening:
  - trust the persisted settings journal only while the setting still reads Lantern's value;
  - a tighter match when looking for old helper processes;
  - uninstall removes the journal.

**Plan 3: integration**
- Phone notifications in KDE. Clicking one opens that app's window.
- Drag and drop files from the PC to the phone, through Android share with a Downloads fallback.
- Phone audio on the PC (one stream).
- Gamepads, following whichever Lantern window is active.
- A tray icon, and pausing a window's encoder while it is minimised.
- A latency harness, with a target of no more than 5 ms glass-to-glass behind scrcpy.

**Later / ideas**
- Wi-Fi or Tailscale transport (the transport is already replaceable).
- An optional per-app `--aspect W:H` for apps that letterbox badly. For example, Microsoft Windows
  App's remote-desktop cursor drifts when its session is letterboxed; until then, use its "Match
  current display" resolution.
- Encoding the full display, with nothing cropped to the 16-pixel alignment at the right and
  bottom edges.
- Wider phone, GPU and desktop support.

Not yet verified:
- a PIN keyguard;
- third-party notification clicks;
- app-initiated rotation on a real device;
- long runs with 5–6 windows;
- interactive play in games.

Design: [`docs/superpowers/specs/2026-11-07-lantern-design.md`](docs/superpowers/specs/2026-11-07-lantern-design.md).
Contributor and agent notes: [`docs/HANDOFF.md`](docs/HANDOFF.md).

## License

Apache-2.0 ([LICENSE](LICENSE)). The vendored scrcpy sources are © Genymobile and Romain Vimont,
Apache-2.0; see `android/helper/vendor/UPSTREAM`.
