# Lantern

Run Android apps from a phone plugged into your PC as ordinary KDE Plasma (Wayland) windows.
Each app gets its own window, taskbar entry, Alt-Tab entry, launcher entry with the phone's icon,
and remembered size. Each app also runs on its own virtual display on the phone, so Lantern
doesn't mirror the phone's screen and the apps don't share one canvas. Close the window and the
app quits on the phone. You type into the apps with the PC keyboard (any language, through the
PC's input method), and text copied on either side can be pasted on the other.

> **Status: pre-release (v0.1.0-alpha).** Lantern is a personal project. It is built and tested on
> one setup: KDE Plasma 6 Wayland on Ubuntu 26.04 with an Intel GPU, plus a Xiaomi 11T Pro on
> Android 17 (LineageOS 24) and a Motorola edge 50 pro on Android 16, over USB. Other phones,
> Android versions, GPUs and desktops may not work. Audio, notifications and file drop are not
> here yet; see the [Roadmap](#roadmap).

## How it works

Two parts, both in the one `lantern` binary:

- `lanternd` (`lantern daemon`, a systemd user service) watches adb for the phone and deploys a
  small helper to it. It keeps one session open while the phone is plugged in.
- `lantern open <package>` is one window. It decodes that app's H.265/H.264 stream with VA-API and
  draws it with wgpu. Mouse, scroll and keys go back to the app.

The phone side is a Java helper started over adb as the `shell` user (`app_process`, no root).
It creates one virtual display and hardware encoder per window, injects input, and syncs the
clipboard. It borrows a few wrappers from [scrcpy](https://github.com/Genymobile/scrcpy) 5.0.1
(Apache-2.0, vendored under `android/helper/vendor`). Next to it runs a small companion app,
`dev.lantern.companion`, which lanternd installs: it lists the phone's launcher apps (with
labels and icons, per profile) and provides the invisible Lantern keyboard that types PC text
into apps. The helper hands the companion a private pipe pair through a shell-only content
provider, after checking the companion's package, uid and signing certificate. The wire
protocol is in [`docs/protocol.md`](docs/protocol.md).

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
- lanternd serves one phone at a time. With several devices connected it keeps the one it is
  serving, else takes the first that came online; when that one goes away it starts the next one
  still online. To pin one, set `LANTERN_SERIAL`: run `systemctl --user edit lanternd` and add
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

`install` builds the phone helper jar and the companion APK (running their host-JVM tests),
embeds both in a release `lantern`, copies that to `~/.local/bin/lantern`, and installs
`~/.config/systemd/user/lanternd.service` and a **Lantern** app-menu entry
(`~/.local/share/applications/dev.lantern.Lantern.desktop`, which runs `systemctl --user start
lanternd.service`, e.g. after Quit in the tray). It then runs `systemctl --user daemon-reload` and
`systemctl --user enable --now lanternd`. Running it again is safe: it restarts a running
daemon only when the binary or the unit changed.

Build only: `python3 tools/build.py helper` (jar), `companion` (APK) or `all` (both +
`cargo build --release`). The companion is signed with a key created on first build at
`~/.local/share/lantern/companion-signing.p12` (`LANTERN_COMPANION_KEY` overrides). The build
pins that key's certificate into the helper, which links only to a companion signed with it.
Tests: `cd host && cargo test`.

## Use

```sh
lantern open com.android.settings     # one window per app; opening it again focuses that window
lantern open --user 10 com.android.settings   # the same app from Android user/profile 10 (e.g. work)
lantern status                        # daemon state as JSON
lantern disconnect                    # end the session (phone restored) and stay disconnected
lantern connect                       # connect again (also: unplug and replug the phone)
```

You rarely need the command: once the phone has connected, every app on its launcher (each
profile's, work apps too) has an entry with its icon in the **Phone** submenu of the KDE
application menu (and in KRunner). The entries follow the phone while it is connected: an app
installed or removed there is added or removed here. They stay when the phone is unplugged, and
the menu keeps the apps of every phone you have connected: switching phones adds the new phone's
apps and leaves the other phone's entries (and your pins) alone. An app both phones have is one
entry. Opening an app while no phone that has it is connected shows a notification "Connect your
Xiaomi 11T Pro" (the phone's name from its settings), or, once the connected phone's app list has
arrived and it lacks the app, which phone has it. They are
`~/.local/share/applications/lantern.*.desktop` (the line `X-Lantern-Serials=` lists the phones),
with icons in `~/.local/share/lantern/icons/`.

`--user N` (default `0`) picks the Android user or profile the app runs as; `adb shell pm list
users` lists them. Each user's copy of an app is its own window: Wayland app_id
`lantern.<pkg>` for user 0 and `lantern.<pkg>-u<N>` otherwise, and its own remembered size. The
title (and menu entry) is the app's name on the phone, with ` (work)` for a work-profile app and
` (user N)` for another user, e.g. `Files (work)`. `<pkg>` must be a package name (letters,
digits, `_`, `.`), and `N` a plain decimal number; anything else is a usage error (exit 2).

Typing: keys go to the app as keys (shortcuts such as Ctrl+C work), and text the US layout can't
produce (another layout, dead keys, an input method such as ibus or fcitx5, emoji) is committed
as text through the Lantern keyboard, including composition (preedit). Clipboard: plain text
copied on the PC can be pasted on the phone and the other way round, up to 1 MiB (unless
`clipboard_sync` is off, see below). Passwords stay
put: text a password manager marks secret (KDE's `x-kde-passwordManagerHint`) and Android
clips marked sensitive are never synced. Neither side logs clipboard or typed text.

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

`lantern status` prints `{"state":"idle|starting|ready|paused","serial":…,"helper":<jar sha256>,"windows":[{"id","pkg","user"}],"companion":"ready|unavailable"}`
(`companion` is omitted until known; `paused` means you disconnected the phone in Lantern).
`lantern connect` and `lantern disconnect` print the same report (the state before the command)
and exit 1 if lanternd isn't running. A disconnected phone stays disconnected until you connect
it again or unplug and replug it; opening an app meanwhile shows "Lantern is disconnected".
Logs: `journalctl --user -u lanternd`. Set `LANTERN_LOG=debug` for more detail.

Exit statuses: 0 ok, 1 runtime failure, 2 usage error, 3 helper protocol mismatch (`daemon`
only; systemd does not restart it).

## Tray and settings

lanternd puts a phone icon in the KDE system tray (when there is a tray; without one it runs as
before). Its menu shows what Lantern is doing ("Connected to Xiaomi 11T Pro", "Reconnecting to …",
"Disconnected from …"), and a line when the companion isn't fully ready (for example "Typing
through the Lantern keyboard is off"). It holds:

- **Disconnect / Connect**: the same as `lantern disconnect` / `lantern connect`.
- **Sync clipboard**, **Type with the Lantern keyboard (from the next connection)**: check items
  over the settings file below.
- **Start Lantern at login**: whether `lanternd.service` is enabled (`systemctl --user
  enable|disable`); not stored in the settings file.
- **Open settings folder**: `~/.config/lantern/` (the file is created with its defaults).
- **Undo phone setup…**: asks first, then ends the session and does what `lantern undo-setup`
  does, for the phone shown; the phone then stays disconnected until replugged or connected.
- **Quit Lantern**: stops lanternd (systemd doesn't restart it). Start it again from the
  **Lantern** entry in the app menu, or `systemctl --user start lanternd`.

Settings live in `~/.config/lantern/settings.toml` (`$XDG_CONFIG_HOME` is honoured). A missing
file or key means the default; edits by hand apply within 2 s, no restart needed:

```toml
clipboard_sync = true     # copy text between the PC and the phone; applies at once
lantern_keyboard = true   # type through the Lantern keyboard; applies at the next connection
```

With `lantern_keyboard = false` the session changes only stay-awake: the phone's keyboard settings
are left alone and PC text can't be typed into phone apps (keys and shortcuts still work).
Disconnect then Connect applies a change at once. A value of the wrong type falls back to that
key's default; a file that doesn't parse gives the defaults and the tray shows "Settings file has
an error; using defaults" and won't overwrite it. When the tray writes the file it keeps unknown
keys but rewrites the header comment and drops any other comments.

## What it does to your phone

Whenever the phone is plugged in and lanternd is running (the service starts with your login
session), lanternd starts a session on the phone at once, even if no window is open:

- **The phone's own screen (panel) is off while a session runs.** The phone stays awake, and
  the apps keep drawing to their Lantern displays. The panel comes back on when the session ends.
- **Two settings are changed for the session and restored after it:**
  - "stay awake while charging" (`settings global stay_on_while_plugged_in`) is set to `7`;
  - the keyboard: the Lantern keyboard (`dev.lantern.companion/.LanternIme`) is added to
    `secure enabled_input_methods` and selected as `default_input_method`. It draws nothing, so
    the phone shows no on-screen keyboard in Lantern windows; PC text reaches apps through it.
    If selecting it fails, the session runs without it (no PC text input, `companion` reads
    `unavailable`), and the phone's own keyboard stays hidden on Lantern displays. With
    `lantern_keyboard = false` the keyboard is not touched at all.

  The original values are restored when the session ends, including when the helper is killed
  or the cable is pulled. A separate guard process on the phone does that restore. As a last line
  for the keyboard, the companion keeps your keyboard's id (also on disk, so it survives a
  reboot) and switches back to it by itself 5 s after it loses the helper, or, after a reboot or battery
  death mid-session, 5 s after the phone is unlocked with the Lantern keyboard still selected.
  If your keyboard is gone or won't take, it picks the first other enabled keyboard instead; only
  when there is no other enabled keyboard at all does the Lantern keyboard stay selected (pick
  one in the phone's settings, or plug in again and let the next session restore it). The
  companion can only switch the selected keyboard; the stay-awake value and the enabled-keyboards
  list wait for the next session after such a reboot. The
  original values are also journaled to `/data/local/tmp/lantern-session.json` before anything
  changes, and the file is deleted after a clean restore. If both helper and guard die without
  restoring (reboot, battery death, `kill -9` of both), the next session takes the originals from
  that file rather than the current values, as long as each setting still holds the value Lantern
  wrote, and restores them when it ends. A setting you changed yourself in the meantime is never
  overwritten. A new session
  also waits (up to 10 s) for the previous session's helper processes to exit, killing any still
  alive after that, so two sessions never overlap on the phone.
- **One virtual display per open window**, named `lantern-<id>-<package>`. Each is removed when its
  window closes, and closing a window quits its app's task (close = quit).
- **The helper jar stays in `/data/local/tmp`** as `lantern-helper-<sha256>.jar`, and the helper
  runs as the `shell` user (`app_process`) only during a session. Every session start deletes
  the jars of other Lantern builds (`lantern-helper-*.jar` other than the current one). lanternd
  also adds one `adb forward` for the session and removes it when the session ends.
- **The companion app `dev.lantern.companion` is installed** (and updated with `lantern`). It has
  no launcher icon and requests no permissions (no network); it can see the launcher apps, and
  only the shell (the helper) can reach its link. It writes no settings; the keyboard fallback
  above is an input method switching itself back.
- **The clipboard is shared while a session runs** (unless `clipboard_sync = false`): a text
  copied on the PC becomes the phone's clipboard, and the other way round (never sensitive or
  password-manager clips).

The phone is restored whenever the session ends: when you stop lanternd
(`systemctl --user stop lanternd`), unplug the phone, the daemon dies, or you log out. Restored
means the panel is on, the stay-awake and keyboard settings have their old values, there are no
Lantern displays
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
                                         # (or "Undo phone setup…" in the tray, lanternd running)
python3 tools/build.py uninstall      # disable --now, undo-setup (best effort), remove the unit
                                      # and ~/.local/bin/lantern
```

lanternd installs the Lantern companion app (`dev.lantern.companion`: keyboard and app list) on the
phone, and keeps it at the build inside `lantern`. The first time it sets up a phone it shows a
notification "Lantern set up your phone" and records the phone in
`~/.local/state/lantern/devices/<serial>.json`. If the install fails, the session runs without the
companion and one notification per daemon run says so (details in `journalctl --user -u lanternd`).

`lantern undo-setup` refuses whenever lanternd answers on its socket, even when idle (it may be
about to retry a session); use the tray's "Undo phone setup…" then. It cannot see a lanternd on another runtime dir or another program using
adb on the phone; stop those first. It restores any
setting a dead session left changed, the keyboard included (only where the setting still holds
Lantern's value), deletes Lantern's files from `/data/local/tmp`, uninstalls the companion,
takes that phone off its KDE menu entries (an entry and its icon go when no other phone lists
the app), and forgets the phone. It prints one
line per step and exits 0 only if every step succeeded. Without `--serial` it uses `$LANTERN_SERIAL`
or the only connected phone.

`uninstall` is idempotent. It runs `lantern undo-setup` after stopping the daemon; if the phone is not
connected then, run it again later (`lantern undo-setup` is gone once `lantern` is removed, so
reinstall or remove the companion with `adb uninstall dev.lantern.companion`).

## Acceptance test (real phone + KDE desktop)

```sh
python3 tools/build.py install && python3 tools/device_test.py   # items 1–7, 9–13, 15–20, unattended
python3 tools/device_test.py --manual                            # adds 8 and 14: USB unplug, prompts
python3 tools/device_test.py --only 12 13                        # some items, by number or name
python3 tools/device_test.py --nested                            # windows in a headless nested KWin
python3 tools/device_test.py --signal=KILL                       # item 7 with SIGKILL
python3 tools/device_test.py --state                             # status + phone readbacks
```

Items: 1 session on plug, 2 open, 3 second open focuses, 4 resize, 5 close quits, 6 app exit
closes the window, 7 daemon kill restores the phone, 8 USB unplug, 9 companion linked, 10 menu
entries follow the phone, 11 title is the app's name, 12 Unicode text and composition reach the
app, 13 clipboard both ways, 14 USB unplug restores the keyboard, 15 another phone's entries
survive this phone's sync, 16 "Connect your …" for another phone's app, 17 clipboard sync off,
18 keyboard setting off, 19 `lantern disconnect`/`connect`, 20 the tray menu (needs the real,
unlocked desktop and its session bus).

The test restarts lanternd. Item 1 stops it first and reads the phone's own
`stay_on_while_plugged_in`, `default_input_method` and `enabled_input_methods`, which later items
expect to see restored; set `LANTERN_STAY_ON_RESTORED=<value>` to give the first instead.
`--signal=KILL` checks that systemd restarts a SIGKILLed lanternd and that the new session
removes the dead daemon's forward. The test opens only `com.android.settings`,
`org.lineageos.jelly` and the companion's test screen (an EditText enabled only during items 10,
12, 13 and 17), briefly, and drives windows through the KWin scripting D-Bus. Items 13 and 17
replace the PC clipboard and put its text back afterwards; items 17 and 18 save
`settings.toml` and restore it (or remove it if it was absent); items 15 and 16 plant and remove
a fake other phone's menu entry and device record. Windows get no frames while the Plasma session is
locked, so the window items fail there; `--nested` runs them in a private headless KWin instead
(the clipboard still goes through the real session).

## Layout

- `android/helper/`: the phone helper (Java, no Gradle; vendored scrcpy 5.0.1 wrappers).
- `android/companion/`: the companion app (app list, Lantern keyboard; APK built without Gradle).
- `host/proto/`: wire protocol (`docs/protocol.md`).
- `host/lantern/`: daemon and window (Rust).
- `tools/`: `build.py` (build, install), `device_test.py` (acceptance).

## Roadmap

Plan 1 (core) is done: per-app windows, video, mouse/scroll/keys, resize with relayout, close = quit,
the phone restoring itself, work profiles, install, and an on-device acceptance suite.

Plan 2 (feels like a desktop app) is done: KDE menu entries and icons that follow the phone's
apps, window titles with the app's name (and "(work)"), typing any text through the hidden
Lantern keyboard, text clipboard both ways, `lantern undo-setup`, and phone-safety hardening of
the settings journal.

Plan 3 (phones and tray) is implemented, with its on-device acceptance run still to do: menu
entries for every phone you use, phone names in "Connect your …" notices, the tray with
Connect/Disconnect, Undo phone setup and Quit, `settings.toml` (clipboard sync, the Lantern
keyboard), start at login, and `lantern connect|disconnect`. Next:

**Plan 4: integration**
- Phone notifications in KDE. Clicking one opens that app's window.
- Drag and drop files from the PC to the phone, through Android share with a Downloads fallback.
- Phone audio on the PC (one stream).
- Gamepads, following whichever Lantern window is active.
- Pausing a window's encoder while it is minimised.
- A latency harness, with a target of no more than 5 ms glass-to-glass behind scrcpy.
- Choosing between two phones plugged in at once, and a settings window.
- Clipboard with a PIN-locked phone (Android returns no clipboard until it is unlocked once), and
  Ctrl shortcuts in Microsoft Windows App's remote sessions.

**Later / ideas**
- Wi-Fi or Tailscale transport (the transport is already replaceable).
- An optional per-app `--aspect W:H` for apps that letterbox badly. For example, Microsoft Windows
  App's remote-desktop cursor drifts when its session is letterboxed; until then, use its "Match
  current display" resolution.
- Encoding the full display, with nothing cropped to the 16-pixel alignment at the right and
  bottom edges.
- Wider phone, GPU and desktop support.

Not yet verified:
- typing through a real PC input method (ibus, fcitx5) and on a real (unlocked) Plasma desktop;
  so far the composition path ran through test hooks in a headless KWin;
- a real password manager's secret hint (tested with a stand-in client, not KeePassXC);
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
