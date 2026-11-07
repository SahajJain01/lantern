# Lantern

Run Android apps from a phone plugged into your PC as ordinary KDE Plasma (Wayland) windows.
Each app gets its own window, taskbar entry and size. It runs on its own virtual display on the
phone, so the phone's own screen is not mirrored.

Two parts, both in the one `lantern` binary:

- `lanternd` (`lantern daemon`, a systemd user service) watches adb for the phone, deploys a small
  helper to it and keeps one session open while it is plugged in.
- `lantern open <package>` is one window. Close the window and the app quits on the phone.

Requirements: KDE Plasma on Wayland, `adb` on `PATH` with the phone authorized for USB debugging
(Android 15+, API 35), VA-API for hardware decoding (software decoding is the fallback), the
Rust toolchain, and the JDK and Android build tools for the helper (`~/.local/opt/pd-toolchain`).
lanternd serves the only connected device. With several devices connected, set
`LANTERN_SERIAL` to the one it should serve (`systemctl --user edit lanternd`, then
`[Service]` / `Environment=LANTERN_SERIAL=<serial>`; `adb devices` lists the serials).

lanternd runs `adb` from the systemd user manager's `PATH`, not your shell's. If `adb` lives
somewhere else (for example `~/Android/Sdk/platform-tools`), add it to that `PATH`: either in
`~/.config/environment.d/*.conf` (`PATH=$HOME/Android/Sdk/platform-tools:$PATH`, applies from the
next login) or with `systemctl --user edit lanternd` and
`Environment=PATH=<dir with adb>:/usr/local/bin:/usr/bin:/bin`. Check with
`systemctl --user show-environment | grep ^PATH`.

## Build and install

```sh
source ~/.local/opt/pd-toolchain/env.sh
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
lantern status                        # daemon state as JSON
```

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
  current value, and restores it when it ends. A new session also waits (up to 10 s) for the
  previous session's helper processes to exit, killing any still alive after that, so two
  sessions never overlap on the phone.
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

python3 tools/build.py uninstall      # disable --now, remove the unit and ~/.local/bin/lantern
adb shell rm -f '/data/local/tmp/lantern-helper-*.jar'   # remove the helper jar from the phone
```

`uninstall` is idempotent. It does not touch the phone beyond stopping the daemon, which restores
the phone. If the phone still shows a changed setting after uninstalling (a session died and none
followed), `adb shell cat /data/local/tmp/lantern-session.json` shows the original; restore it with
`adb shell settings put global stay_on_while_plugged_in <old value>` (Android's default is `0`)
and delete that file.

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
