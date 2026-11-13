#!/usr/bin/env python3
"""Lantern acceptance suite on a real phone + KDE Plasma (Wayland) desktop.

    python3 tools/build.py install && python3 tools/device_test.py   # items 1-7, 9-13, 15-20, 22-25
    python3 tools/device_test.py --manual                            # also 8, 14, 21, 26 (prompts)
    python3 tools/device_test.py --only 12 13                        # by number or name
    python3 tools/device_test.py --nested                            # windows in a nested KWin
    python3 tools/device_test.py --state                             # print phone state

Items (26; 22 automatic, 8, 14, 21 and 26 only with --manual):
     1 session_starts_on_plug              11 title_is_label
     2 open_settings_creates_one_display   12 ime_unicode_text
     3 second_open_focuses                 13 clipboard_both_ways
     4 resize_reflows                      14 usb_yank_restores_keyboard (manual)
     5 close_quits                         15 phones_keep_their_entries
     6 app_exit_closes_window              16 not_on_this_phone_notice
     7 daemon_kill_restores_phone          17 clipboard_setting_off
     8 usb_yank (manual)                   18 keyboard_setting_off
     9 companion_linked                    19 disconnect_and_connect
    10 catalogue_tracks_phone              20 tray_menu (real session bus, unlocked desktop)
                                           21 unlock_prompt (manual, PIN phone, Sahaj types)
                                           22 swipe_phone_never_prompts (Xiaomi, no PIN)
                                           23 per_phone_folders (restarts lanternd)
                                           24 autoconnect_on_remembers_off (adb reconnect)
                                           25 autoconnect_off_asks_on_plug (adb reconnect, tray)
                                           26 connect_from_entry_opens_app (manual, Sahaj clicks)
A full automatic run prints `22/22 PASS` when everything passes.

lanternd serves only LANTERN_SERIAL's phone during the run: the suite sets
`LANTERN_SERIAL=<SERIAL>` in the systemd user environment (restarting lanternd) if it isn't set
there, and puts the previous value back (restarting again) at the end. Item 24 lifts it for its
own steps, and so do items 25 and 26.

Needs: lanternd installed as the systemd user unit (tools/build.py install), the phone
(LANTERN_SERIAL, default 20e9318) authorized over USB, KWin's scripting D-Bus (qdbus6) and
Klipper's D-Bus (items 13, 17, 21, 22). Opens only com.android.settings, org.lineageos.jelly and the
companion's test screen (enabled only during items 10, 12, 13, 17, 21 and 22), each for a few
seconds, and touches no other window. Items 13, 17, 21 and 22 save the PC clipboard's text and put it back
afterwards; items 17 and 18 also save ~/.config/lantern/settings.toml and restore it (or remove it
if it was absent); item 18 restarts lanternd with the keyboard off and back on; item 19
disconnects the phone in Lantern for ~15 s and connects it again; item 20 does the same through
the tray menu over the session bus (and saves a screenshot of the screen to the log dir); item 24
sets `autoconnect = true` in settings.toml (restored afterwards), drops the phone off adb with
`adb reconnect` twice and restarts lanternd; item 25 sets `autoconnect = false` (restored),
drops the phone off adb once and connects it through its tray row; item 26 disconnects the
phone, refuses an open of Settings and has Sahaj click Connect on the "Connect your <phone>"
notification (real desktop only), then closes the Settings window it opened. Items 21, 22, 25
and 26 log the session bus's Notify calls to the log dir and print only matched summaries.
Prints one PASS/FAIL line per item; exits 1 if any item failed.

The window items (2-6, 11-13, 17, 22) need frames from the compositor, which a locked Plasma session
does not give. --nested (or LANTERN_TEST_NESTED=1) runs every test window in a private headless
`kwin_wayland --virtual` (no Xwayland, no DISPLAY) on its own D-Bus instead, with scratch XDG
config/data/state/cache and runtime dirs (removed afterwards, with every service that bus
activated); the windows keep the real XDG_RUNTIME_DIR and reach it by absolute WAYLAND_DISPLAY.
That compositor gives no keyboard focus, so the text items prepend the window's `focus` debug
step there. The clipboard still goes through the real session (lanternd's clipboard client and
Klipper).
"""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime

SERIAL = os.environ.get("LANTERN_SERIAL") or "20e9318"
LANTERN = shutil.which("lantern") or str(Path.home() / ".local/bin/lantern")
UNIT = "lanternd.service"
SETTINGS = "com.android.settings"
JELLY = "org.lineageos.jelly"
COMPANION = "dev.lantern.companion"
LANTERN_IME = "dev.lantern.companion/.LanternIme"
# The phone's own stay_on_while_plugged_in value, which a session must restore. Item 1 reads it
# with the daemon stopped (or set LANTERN_STAY_ON_RESTORED to skip that).
STAY_ON_RESTORED = os.environ.get("LANTERN_STAY_ON_RESTORED") or None
# The keyboard settings a session must restore, snapshotted with the stay-awake baseline.
IME_RESTORED = None   # secure default_input_method
IMES_RESTORED = None  # secure enabled_input_methods
STAY_ON_SESSION = "7"  # Session.STAY_ON in the helper
JOURNAL = "/data/local/tmp/lantern-session.json"  # JournalStore.PATH in the helper
RESIZE_LOGICAL = 900
KILL_SIGNAL = "TERM"  # item 7; --signal=KILL
MANUAL_RUN = False  # --manual: replug() asks for a real unplug
LOG_DIR = Path(tempfile.mkdtemp(prefix="lantern-device-test-"))
NESTED = None  # the Nested compositor while --nested
IME_TEXT = "Tiếng Việt 👍việ"  # item 12: the EditText after text + compose + finish
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class Fail(Exception):
    pass


def check(cond, msg):
    if not cond:
        raise Fail(msg)


def note(msg):
    print(f"    {msg}", flush=True)


def sh(*args, timeout=20, check_rc=True, env=None):
    r = subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=timeout,
                       encoding="utf-8", errors="replace", env=env)
    if check_rc and r.returncode != 0:
        raise Fail(f"{' '.join(map(str, args))} -> {r.returncode}: {r.stderr.strip()}")
    return r.stdout


def adb(*args, timeout=20):
    return sh("adb", "-s", SERIAL, *args, timeout=timeout).replace("\r", "")


def wait_until(what, timeout, fn, interval=0.1):
    """Polls fn until it returns a truthy value; returns (value, seconds)."""
    t0 = time.monotonic()
    last = None
    while True:
        try:
            last = fn()
        except Fail as e:
            last = None
            err = e
        else:
            err = None
        if last:
            return last, time.monotonic() - t0
        if time.monotonic() - t0 > timeout:
            raise Fail(f"{what}: not within {timeout} s" + (f" ({err})" if err else ""))
        time.sleep(interval)


# --- lantern / systemd ---------------------------------------------------------------------

def status():
    try:
        r = subprocess.run([LANTERN, "status"], capture_output=True, text=True, timeout=10)
    except OSError as e:
        raise Fail(f"lantern status: {e}")
    if r.returncode != 0:
        raise Fail(f"lantern status -> {r.returncode}: {r.stderr.strip()}")
    return json.loads(r.stdout)


def phone_status(serial=None):
    """`serial`'s entry in `lantern status`'s `phones` (default SERIAL), or {}."""
    serial = serial or SERIAL
    for phone in status().get("phones", []):
        if phone.get("serial") == serial:
            return phone
    return {}


def device_record(serial=None):
    """`serial`'s device record (default SERIAL) as a dict, {} if absent or unreadable."""
    path = devices_dir() / f"{re.sub(r'[^A-Za-z0-9._:-]', '_', serial or SERIAL)}.json"
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


def lantern_cmd(cmd, serial=None):
    """`lantern connect|disconnect [serial]`: the status report lanternd answered with (as
    JSON)."""
    args = [LANTERN, cmd] + ([serial] if serial else [])
    r = subprocess.run(args, capture_output=True, text=True, timeout=10)
    if r.returncode != 0:
        raise Fail(f"lantern {cmd} -> {r.returncode}: {r.stderr.strip()}")
    return r.stdout.strip()


def systemctl(*args, check_rc=True):
    return sh("systemctl", "--user", *args, timeout=60, check_rc=check_rc)


def wait_ready(timeout=10):
    """Waits until lanternd serves SERIAL's phone."""
    st, secs = wait_until("daemon ready", timeout,
                          lambda: (s := status())["state"] == "ready"
                          and s["serial"] == SERIAL and s)
    return st, secs


def systemd_env():
    """The systemd user manager's environment as a dict."""
    env = {}
    for line in systemctl("show-environment").splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            env[k] = v
    return env


class manager_env:
    """Changes the systemd user environment (`set` values, `unset` names) and restarts
    lanternd; on exit puts every changed name back as it was (or unsets it) and restarts
    lanternd again. Each restore step is guarded."""

    def __init__(self, set={}, unset=[]):
        self.set, self.unset = dict(set), list(unset)
        self.saved = {}

    def __enter__(self):
        env = systemd_env()
        self.saved = {k: env.get(k) for k in [*self.set, *self.unset]}
        try:
            if self.set:
                systemctl("set-environment", *(f"{k}={v}" for k, v in self.set.items()))
            if self.unset:
                systemctl("unset-environment", *self.unset)
            note(f"lanternd environment: set {self.set} unset {self.unset}")
            systemctl("restart", UNIT)
        except BaseException:
            # A `with` never calls __exit__ when __enter__ raises: put everything back here.
            self.__exit__(*sys.exc_info())
            raise
        return self

    def __exit__(self, *exc):
        steps = []
        for k, v in self.saved.items():
            if v is None:
                steps.append((f"unset {k}", lambda k=k: systemctl("unset-environment", k)))
            else:
                steps.append((f"put back {k}", lambda k=k, v=v: systemctl("set-environment",
                                                                         f"{k}={v}")))
        steps.append(("restart lanternd", lambda: systemctl("restart", UNIT)))
        cleanup(exc[0] is None, *steps)
        return False


def adb_state(serial=None):
    """`serial`'s state in `adb devices` (`device`, `offline`, ...), None if not listed."""
    for line in sh("adb", "devices", timeout=10).splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 2 and parts[0] == (serial or SERIAL):
            return parts[1]
    return None


def replug(manual=None):
    """Drops the phone off adb and back: `adb -s SERIAL reconnect`, which must take it away
    (missing or `offline`) within 5 s, then back as `device` within 15 s. Under --manual
    Sahaj pulls the cable and plugs it back in instead."""
    manual = MANUAL_RUN if manual is None else manual
    if manual:
        input(f">>> Pull {SERIAL}'s USB cable, wait 15 s, plug it back in, then press Enter: ")
    else:
        sh("adb", "-s", SERIAL, "reconnect", timeout=15, check_rc=False)
        try:
            wait_until("phone off adb", 5, lambda: adb_state() != "device", interval=0.1)
        except Fail:
            raise Fail("adb reconnect did not drop the phone; run --manual and pull the cable")
    _, secs = wait_until("phone back on adb", 15, lambda: adb_state() == "device", interval=0.2)
    note(f"phone back on adb after {secs:.1f} s")


def device_name():
    """The phone's friendly name (Settings > About), else its serial."""
    return adb("shell", "settings", "get", "global", "device_name").strip() or SERIAL


class NotifyMonitor:
    """`busctl --user monitor org.freedesktop.Notifications` into LOG_DIR/notifications-<tag>.log
    while open. `notifies()` lists every Notify call seen so far as (summary, actions); callers
    print only the summaries they match."""

    def __init__(self, tag):
        self.log = LOG_DIR / f"notifications-{tag}.log"
        self.f = None
        self.p = None

    def __enter__(self):
        self.f = open(self.log, "w")
        self.p = subprocess.Popen(["busctl", "--user", "monitor", "org.freedesktop.Notifications"],
                                  stdout=self.f, stderr=subprocess.STDOUT)
        time.sleep(1)  # the monitor is subscribed
        return self

    def __exit__(self, *exc):
        self.p.terminate()
        try:
            self.p.wait(5)
        except subprocess.TimeoutExpired:
            self.p.kill()
        self.f.close()
        return False

    def notifies(self):
        out = []
        for block in self.log.read_text(errors="replace").split("\u2023"):
            if "Member=Notify" not in block:
                continue
            strings = re.findall(r'STRING "((?:[^"\\]|\\.)*)";', block)
            arr = re.search(r'ARRAY "s" \{(.*?)\};', block, re.S)
            actions = re.findall(r'STRING "((?:[^"\\]|\\.)*)";', arr.group(1)) if arr else []
            if len(strings) >= 4:
                out.append((strings[2], actions))  # app, icon, summary, body, ...
        return out

    def matching(self, summary, since=0):
        return [n for n in self.notifies()[since:] if n[0] == summary]


def tray_name():
    """lanternd's StatusNotifierItem bus name (take it after any lanternd restart)."""
    pid = systemctl("show", "-p", "MainPID", "--value", UNIT).strip()
    check(pid not in ("", "0"), f"{UNIT} is not running")
    return f"org.kde.StatusNotifierItem-{pid}-1"


def tray_rows(name):
    """The tray menu's top-level rows: {id: (label, toggle-state or None)}."""
    out = sh("busctl", "--user", "--json=short", "call", name, "/MenuBar",
             "com.canonical.dbusmenu", "GetLayout", "iias", "--", "0", "-1", "0")
    _, (_, _, children) = json.loads(out)["data"]
    found = {}
    for child in children:
        cid, props, _ = child["data"]
        found[cid] = (props.get("label", {}).get("data", ""),
                      props.get("toggle-state", {}).get("data"))
    return found


def tray_click(name, cid):
    sh("busctl", "--user", "call", name, "/MenuBar", "com.canonical.dbusmenu", "Event",
       "isvu", "--", str(cid), "clicked", "i", "0", "0")


def tray_phone_row(name, device):
    """(row id, rows) of the checkmark row named `device` (ids 100n), else None."""
    want = device.replace("_", "__")  # dbusmenu escapes mnemonics
    r = tray_rows(name)
    ids = [cid for cid, (label, toggle) in r.items()
           if label == want and toggle is not None and cid >= 100 and cid % 100 == 0]
    return (ids[0], r) if ids else None


def edit_settings(saved_file, key, value):
    """Writes settings.toml as `saved_file` (bytes or None) with `key = value` instead of any
    line setting `key`."""
    path = settings_file()
    text = (saved_file or b"").decode(errors="replace")
    lines = [l for l in text.splitlines() if not re.match(rf"\s*{key}\s*=", l)]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(lines + [f"{key} = {value}"]) + "\n")


class Window:
    """One `lantern open <pkg>` process, stderr logged to a file."""

    every = []  # all windows started, so the suite can kill whatever is left on any exit

    def __init__(self, pkg, tag, debug_input=None):
        Window.every.append(self)
        self.pkg = pkg
        self.log = LOG_DIR / f"{tag}.log"
        env = dict(os.environ, LANTERN_LOG="info", NO_COLOR="1")
        if NESTED:
            # The real XDG_RUNTIME_DIR (lanternd's socket), the nested compositor by absolute
            # path, and no DISPLAY, so a window can never fall back onto the real desktop.
            env["WAYLAND_DISPLAY"] = NESTED.socket_path
            env.pop("DISPLAY", None)
        if debug_input:
            env["LANTERN_DEBUG_INPUT"] = debug_input
        self.f = open(self.log, "w")
        self.p = subprocess.Popen([LANTERN, "open", pkg], stdout=self.f, stderr=self.f, env=env)

    def lines(self):
        return [ANSI.sub("", l) for l in self.log.read_text(errors="replace").splitlines()]

    def find(self, event, after=None):
        """Log lines whose message is `event`, as (utc_ts, {field: value})."""
        out = []
        for line in self.lines():
            m = re.match(r"(\S+Z)\s+\w+\s+\S+: (\w[\w ;-]*?)((?: \w+=\S+)*)$", line)
            if not m or m.group(2) != event:
                continue
            ts = datetime.fromisoformat(m.group(1).replace("Z", "+00:00")).timestamp()
            if after is not None and ts < after:
                continue
            fields = dict(kv.split("=", 1) for kv in m.group(3).split())
            out.append((ts, fields))
        return out

    def wait_exit(self, timeout):
        try:
            return self.p.wait(timeout)
        except subprocess.TimeoutExpired:
            raise Fail(f"{self.pkg} window still running after {timeout} s")

    def kill(self):
        if self.p.poll() is None:
            self.p.terminate()
            try:
                self.p.wait(3)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait(3)
        self.f.close()


# --- phone readbacks -----------------------------------------------------------------------

def lantern_displays():
    out = adb("shell", "cmd", "display", "get-displays")
    return re.findall(r'Display id (\d+): DisplayInfo\{"(lantern-[^"]*)"', out)


def display_ids():
    return re.findall(r"Display id (\d+):", adb("shell", "cmd", "display", "get-displays"))


def tasks_off_display0(pkg):
    """Task lines for `pkg` in root tasks on any display other than 0."""
    found, display = [], None
    for line in adb("shell", "am", "stack", "list").splitlines():
        m = re.match(r"RootTask id=\d+ .*displayId=(\d+)", line)
        if m:
            display = m.group(1)
        elif display not in (None, "0") and re.search(rf"taskId=\d+: {re.escape(pkg)}/", line):
            found.append(f"display {display}: {line.strip()}")
    return found


def phone_state():
    panel = re.findall(r"powerMode=(\w+)", adb("shell", "dumpsys", "SurfaceFlinger", "--displays"))
    forwards = [l for l in sh("adb", "forward", "--list").splitlines()
                if l.startswith(SERIAL) and "lantern" in l]
    helpers = adb("shell", "pgrep -f 'dev[.]lantern[.]helper' || true").split()
    return {
        "stay_on": adb("shell", "settings", "get", "global", "stay_on_while_plugged_in").strip(),
        "ime": adb("shell", "settings", "get", "secure", "default_input_method").strip(),
        "imes": adb("shell", "settings", "get", "secure", "enabled_input_methods").strip(),
        "panel": panel[0] if panel else "?",
        "displays": display_ids(),
        "forwards": forwards,
        "helper_pids": helpers,
        # The session journal exists exactly while a session is live (or after one nothing
        # could restore).
        "journal": adb("shell", f"ls {JOURNAL} 2>/dev/null || true").strip() == JOURNAL,
    }


def restored(st):
    base = baseline()
    return (st["stay_on"] == base["stay_on"] and st["ime"] == base["ime"]
            and st["imes"] == base["imes"] and st["panel"] == "On"
            and st["displays"] == ["0"] and not st["forwards"] and not st["helper_pids"]
            and not st["journal"])


def released(st):
    """No session on the phone (the guard, which restores the setting, has exited too)."""
    return (st["panel"] == "On" and st["displays"] == ["0"] and not st["forwards"]
            and not st["helper_pids"])


def read_stay_on_baseline():
    """Stops lanternd, reads the phone's own stay-awake and keyboard values (the restore
    targets), and leaves lanternd stopped."""
    global STAY_ON_RESTORED, IME_RESTORED, IMES_RESTORED
    systemctl("stop", UNIT)
    st, _ = wait_until("phone released after stop", 10, lambda: released(s := phone_state()) and s,
                       interval=0.2)
    if STAY_ON_RESTORED is None:
        STAY_ON_RESTORED = st["stay_on"]
    if IME_RESTORED is None:
        IME_RESTORED, IMES_RESTORED = st["ime"], st["imes"]
    note(f"baseline with lanternd stopped: stay_on={st['stay_on']} ime={st['ime']} "
         f"imes={st['imes']} (restore targets: {STAY_ON_RESTORED}, {IME_RESTORED}, "
         f"{IMES_RESTORED})")
    check(LANTERN_IME not in str(IME_RESTORED) and LANTERN_IME not in str(IMES_RESTORED),
          f"the Lantern keyboard is still selected or enabled with no session: {st}")


def baseline():
    """The phone's own values a session must restore: {stay_on, ime, imes}."""
    if STAY_ON_RESTORED is None or IME_RESTORED is None:
        try:
            read_stay_on_baseline()
        finally:
            systemctl("start", UNIT)
        wait_ready(15)
    return {"stay_on": str(STAY_ON_RESTORED), "ime": IME_RESTORED, "imes": IMES_RESTORED}


def stay_on_baseline():
    return baseline()["stay_on"]


def phone_now():
    """The phone's clock (epoch seconds), for reading logcat from a point on."""
    return int(adb("shell", "date +%s").strip())


def logcat_since(since, tags):
    """`(epoch, message)` for the given logcat tags, at or after phone time `since`."""
    out = []
    for line in adb("logcat", "-d", "-v", "epoch", "-s", *tags, timeout=30).splitlines():
        m = re.match(r"\s*(\d+\.\d+)\s+\d+\s+\d+\s+\w\s+(\S+)\s*: (.*)$", line)
        if m and float(m.group(1)) >= since:
            out.append((float(m.group(1)), m.group(3)))
    return out


def test_activity(on):
    """Enables (or returns to disabled) the companion's test screen via its shell-only
    TEST_ACTIVITY broadcast; the shell may not `pm enable` another package's component."""
    out = adb("shell", "am", "broadcast", "--user", "0", "-n", f"{COMPANION}/.LinkReceiver",
              "-a", f"{COMPANION}.TEST_ACTIVITY", "--es", "enabled", "true" if on else "false")
    check("result=1" in out, f"TEST_ACTIVITY enabled={on}: {out.strip()}")


def test_activity_listed():
    out = adb("shell", "pm", "query-activities", "--user", "0", "-a", "android.intent.action.MAIN",
              "-c", "android.intent.category.LAUNCHER", COMPANION)
    return "TestActivity" in out


def applications_dir():
    data = os.environ.get("XDG_DATA_HOME", "")
    return (Path(data) if data.startswith("/") else Path.home() / ".local/share") / "applications"


def menus_dir():
    """The merged menu files (`catalog::Dirs::menus`)."""
    config = os.environ.get("XDG_CONFIG_HOME", "")
    return (Path(config) if config.startswith("/") else Path.home() / ".config") / \
        "menus" / "applications-merged"


def directories_dir():
    """The menu folder files (`catalog::Dirs::directories`)."""
    data = os.environ.get("XDG_DATA_HOME", "")
    return (Path(data) if data.startswith("/") else Path.home() / ".local/share") / \
        "desktop-directories"


def lantern_state_dir():
    """lanternd's state dir (`companion::state_dir()`)."""
    state = os.environ.get("XDG_STATE_HOME", "")
    return (Path(state) if state.startswith("/") else Path.home() / ".local/state") / "lantern"


def devices_dir():
    """lanternd's device records (`companion::state_dir()/devices`)."""
    return lantern_state_dir() / "devices"


def windows_json():
    """The windows' remembered sizes (`window::sizes_path`)."""
    return lantern_state_dir() / "windows.json"


def size_key_of(pkg):
    """`Naming::size_key(pkg, 0, SERIAL)`: the bare package when menu.json's primary is SERIAL,
    else `<pkg>@<SERIAL>`."""
    try:
        primary = json.loads((lantern_state_dir() / "menu.json").read_text()).get("primary")
    except (OSError, ValueError, AttributeError):
        primary = None
    return pkg if primary == SERIAL else f"{pkg}@{SERIAL}"


def settings_file():
    """lanternd's settings file (`settings::default_path()`)."""
    config = os.environ.get("XDG_CONFIG_HOME", "")
    return (Path(config) if config.startswith("/") else Path.home() / ".config") / \
        "lantern" / "settings.toml"


def read_keys(path):
    """The [Desktop Entry] keys of the file at `path` (first value of each key)."""
    keys = {}
    for line in path.read_text(errors="replace").splitlines():
        if "=" in line and not line.startswith(("#", "[")):
            k, v = line.split("=", 1)
            keys.setdefault(k.strip(), v.strip())
    return keys


def tag(serial):
    """`naming::tag`: `[A-Za-z0-9_]` kept, any other character `_`, at most 64 chars."""
    return "".join(c if (c.isascii() and c.isalnum()) or c == "_" else "_" for c in serial[:64])


def entry_for(serial, pkg, user=0):
    """The (path, keys) of the `lantern.*.desktop` whose X-Lantern-Serial, X-Lantern-Package
    and X-Lantern-User are `serial`, `pkg` and `user`, or None."""
    for path in sorted(applications_dir().glob("lantern.*.desktop")):
        try:
            keys = read_keys(path)
        except OSError:
            continue
        if (keys.get("X-Lantern-Serial") == serial and keys.get("X-Lantern-Package") == pkg
                and keys.get("X-Lantern-User") == str(user)):
            return path, keys
    return None


def app_id_of(pkg):
    """The app_id of `pkg` (user 0) on SERIAL: its menu entry's stem, else `lantern.<pkg>`."""
    found = entry_for(SERIAL, pkg)
    return found[0].stem if found else f"lantern.{pkg}"


def per_phone_entry(pkg, serial, name):
    """The text `catalog::desktop_file` writes for `pkg` (user 0) on non-primary phone `serial`
    (a bare `[A-Za-z0-9._:-]` serial): (file name, text)."""
    t = tag(serial)
    stem = f"lantern.{pkg}-p{t}"
    text = ("[Desktop Entry]\nType=Application\nVersion=1.5\n"
            f"Name={name}\nExec=\"{LANTERN}\" open --phone {serial} {pkg}\n"
            f"Categories=X-Lantern-{t};\nStartupWMClass={stem}\nStartupNotify=true\n"
            f"X-Lantern-Serial={serial}\nX-Lantern-Package={pkg}\nX-Lantern-User=0\n")
    return f"{stem}.desktop", text


def klipper(*args):
    """Klipper on the real session bus (never the nested one). A failure names only the method:
    the arguments may be the user's clipboard text, which is never logged."""
    try:
        r = subprocess.run(["qdbus6", "org.kde.klipper", "/klipper", *args], capture_output=True,
                           text=True, timeout=20, encoding="utf-8", errors="replace")
    except (OSError, subprocess.TimeoutExpired) as e:
        raise Fail(f"klipper {args[0]}: {type(e).__name__}")
    if r.returncode != 0:
        raise Fail(f"klipper {args[0]} -> {r.returncode}")
    return r.stdout


def cleanup(ok, *steps):
    """Runs every (what, fn) cleanup step even if an earlier one fails. If the item itself
    failed (`ok` false) the step errors are only noted, so they never mask its real failure;
    otherwise the first one is raised."""
    errors = []
    for what, fn in steps:
        try:
            fn()
        except Exception as e:
            errors.append(f"{what}: {type(e).__name__}: {e}")
            note(f"cleanup failed: {errors[-1]}")
    if ok and errors:
        raise Fail("cleanup: " + "; ".join(errors))


def armed(st):
    """The phone as a ready session with no windows leaves it."""
    return (st["stay_on"] == STAY_ON_SESSION and st["panel"] == "Off" and st["displays"] == ["0"]
            and len(st["forwards"]) == 1 and st["helper_pids"] and st["journal"])


# --- KWin scripting ------------------------------------------------------------------------

class Nested:
    """A private headless KWin (`--virtual`) on its own D-Bus, for a locked desktop."""

    def __init__(self):
        self.log = LOG_DIR / "nested-kwin.log"
        self.bus_pid = self.kwin = self.f = None
        # Scratch XDG config/data/state/cache dirs for the bus and KWin, so nothing they write
        # (kwinoutputconfig.json's Virtual-0 output, kwinrc, caches) lands in the user's real
        # ones, and a scratch XDG_RUNTIME_DIR (0700), so nothing the private bus activates (the
        # document portal's FUSE mount, the at-spi bus, ksecretd, dconf) binds or replaces a real
        # session socket under /run/user. No DISPLAY either: the nested KWin runs without Xwayland
        # and nothing it starts may reach the real X server.
        self.home = Path(tempfile.mkdtemp(prefix="lantern-nested-kwin-"))
        self.runtime = self.home / "runtime"
        self.runtime.mkdir(mode=0o700)
        os.chmod(self.runtime, 0o700)  # mkdir's mode is masked by the umask
        self.socket = f"lantern-test-{os.getpid()}"
        # Absolute, for the `lantern open` windows: they keep the real XDG_RUNTIME_DIR (lanternd's
        # socket is there) and reach the nested compositor by path.
        self.socket_path = str(self.runtime / self.socket)
        self.env = dict(os.environ)
        for var in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY", "XAUTHORITY",
                    "DBUS_SESSION_BUS_ADDRESS"):
            self.env.pop(var, None)
        self.env["XDG_RUNTIME_DIR"] = str(self.runtime)
        for var in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"):
            d = self.home / var.lower()
            d.mkdir()
            self.env[var] = str(d)
        try:
            out = sh("dbus-daemon", "--session", "--fork", "--print-address=1", "--print-pid=1",
                     env=self.env)
            addr, pid = out.split()[:2]
            self.bus_pid = int(pid)
            self.env["DBUS_SESSION_BUS_ADDRESS"] = addr
            self.f = open(self.log, "w")
            # No --xwayland: the nested KWin starts no X server.
            self.kwin = subprocess.Popen(
                ["kwin_wayland", "--virtual", "--no-lockscreen", "--socket", self.socket,
                 "--width", "1920", "--height", "1080"],
                stdout=self.f, stderr=self.f, env=self.env, start_new_session=True)
            sock = Path(self.socket_path)
            wait_until("nested KWin up", 10, lambda: sock.exists() and sh(
                "qdbus6", "org.kde.KWin", "/Scripting", check_rc=False, env=self.env),
                interval=0.2)
        except BaseException:
            self.stop()  # a half-started bus or KWin never outlives a failed start
            raise

    def output(self):
        return self.log.read_text(errors="replace")

    def stop(self):
        if self.kwin and self.kwin.poll() is None:
            self.kwin.terminate()
            try:
                self.kwin.wait(5)
            except subprocess.TimeoutExpired:
                self.kwin.kill()
                self.kwin.wait(5)
        if self.f:
            self.f.close()
        # Services the private bus activated (xdg-desktop-portal-kde, ksecretd, ...) outlive it,
        # and could recreate files in the scratch dirs. They inherited its environment, so the
        # unique scratch path identifies exactly them.
        self._reap()
        if self.bus_pid and self._ours(self.bus_pid):
            try:
                os.kill(self.bus_pid, 15)
            except OSError:
                pass
        self._reap()
        shutil.rmtree(self.home, ignore_errors=True)

    def _ours(self, pid):
        """True if `pid` is ours and its environment carries the scratch path (so the private
        bus or KWin started it). Read fresh: a pid can exit and be reused at any time."""
        if pid == os.getpid():
            return False
        d = Path("/proc") / str(pid)
        try:
            return (d.stat().st_uid == os.getuid()
                    and str(self.home).encode() in (d / "environ").read_bytes())
        except OSError:
            return False

    def _started_here(self):
        return [int(d.name) for d in Path("/proc").iterdir()
                if d.name.isdigit() and self._ours(int(d.name))]

    def _reap(self):
        pids = self._started_here()
        for sig in (15, 9):
            for pid in pids:
                if not self._ours(pid):  # re-validated right before each kill (pid reuse)
                    continue
                try:
                    os.kill(pid, sig)
                except OSError:
                    pass
            t0 = time.monotonic()
            while (pids := self._started_here()) and time.monotonic() - t0 < 3:
                time.sleep(0.1)
            if not pids:
                return
        note(f"nested session processes still running: {pids}")


def kwin(js, name):
    """Runs a KWin script; returns the lines it print()ed (tagged), read back from the journal
    (or from the nested KWin's log)."""
    tag = f"LANTERN-T-{os.getpid()}-{time.monotonic_ns()}"
    path = LOG_DIR / f"{name}.js"
    path.write_text(js.replace("TAG", json.dumps(tag)))
    since = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    env = NESTED.env if NESTED else None

    def printed():
        text = NESTED.output() if NESTED else sh(
            "journalctl", "--user", "--since", since, "--no-pager", "-o", "cat")
        return [l.split(tag, 1)[1].strip() for l in text.splitlines() if tag in l]
    sh("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", name,
       check_rc=False, env=env)
    sid = sh("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript",
             path, name, env=env).strip()
    sh("qdbus6", "org.kde.KWin", f"/Scripting/Script{sid}", "org.kde.kwin.Script.run", env=env)
    try:
        out, _ = wait_until(f"kwin script {name} output", 3, printed)
    finally:
        sh("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", name,
           check_rc=False, env=env)
    return out


FIND = """
const wins = (workspace.windowList ? workspace.windowList() : workspace.clientList())
    .filter(w => String(w.resourceClass) === CLASS);
print(TAG + " count=" + wins.length);
"""

DESCRIBE = FIND + """
for (const w of wins) {
    const c = w.clientGeometry, f = w.frameGeometry;
    print(TAG + " resourceClass=" + w.resourceClass + " desktopFileName=" + w.desktopFileName
          + " caption=" + w.caption + " client=" + c.width + "x" + c.height
          + " frame=" + f.width + "x" + f.height
          + " output=" + (w.output ? w.output.name : "?")
          + " scale=" + (w.output ? w.output.devicePixelRatio : "?"));
}
"""

RESIZE = FIND + """
for (const w of wins) {
    const c = w.clientGeometry, f = w.frameGeometry;
    const dw = f.width - c.width, dh = f.height - c.height;
    w.frameGeometry = {x: f.x, y: f.y, width: WIDTH + dw, height: HEIGHT + dh};
    print(TAG + " resized client " + c.width + "x" + c.height + " -> " + w.clientGeometry.width
          + "x" + w.clientGeometry.height + " scale=" + (w.output ? w.output.devicePixelRatio : "?"));
}
"""

CLOSE = FIND + """
for (const w of wins) { w.closeWindow(); print(TAG + " closed " + w.resourceClass); }
"""


def kwin_on(pkg, js, name, **subst):
    """Runs `js` on the windows of `pkg` on SERIAL: CLASS is its app_id (`app_id_of`), which a
    window without --phone has only if it resolved SERIAL and named itself after its entry."""
    js = js.replace("CLASS", json.dumps(app_id_of(pkg)))
    for k, v in subst.items():
        js = js.replace(k, str(v))
    return kwin(js, name)


# --- the items -----------------------------------------------------------------------------

class Suite:
    def __init__(self):
        self.settings = None

    def session_starts_on_plug(self):
        # Stop the daemon (reading the phone's own stay-awake value while no session runs), then
        # start it so the session start is observed from scratch.
        try:
            read_stay_on_baseline()
        finally:
            systemctl("start", UNIT)
        sh("adb", "-s", SERIAL, "wait-for-device", timeout=60)
        st, secs = wait_ready(10)
        note(f"ready {secs:.1f} s after wait-for-device: {json.dumps(st)}")
        check(st["serial"] == SERIAL and st["helper"] and st["windows"] == [], f"status {st}")
        ph, _ = wait_until("phone armed", 5, lambda: armed(s := phone_state()) and s)
        note(f"phone during the session: {ph}")

    def open_settings_creates_one_display(self):
        self.settings = w = Window(SETTINGS, "settings")
        wait_until("opened", 15, lambda: w.find("opened"))
        _, opened = w.find("opened")[0]
        note(f"window log: opened {opened}")
        st = status()
        note(f"status: {json.dumps(st)}")
        check([x["pkg"] for x in st["windows"]] == [SETTINGS], f"status windows {st['windows']}")
        check(w.p.poll() is None, "window process exited")
        displays = lantern_displays()
        note(f"lantern displays: {displays}")
        check(len(displays) == 1, f"expected 1 lantern display, got {displays}")
        check(displays[0][0] == opened["phone_display"], "display id differs from opened")
        # The surface maps after the first decoded frame; give KWin a moment to list it.
        desc, _ = wait_until("KWin lists the window", 5,
                             lambda: (d := kwin_on(SETTINGS, DESCRIBE, "lantern_describe"))
                             and "count=1" in d[0] and d, interval=0.3)
        for line in desc:
            note(f"kwin: {line}")
        check("count=1" in desc[0], f"expected one KWin window: {desc}")
        # The window was opened without --phone: it resolved SERIAL and took its entry's name.
        want = app_id_of(SETTINGS)
        check(f"resourceClass={want} " in desc[1], f"app_id mismatch: want {want}: {desc[1]}")

    def second_open_focuses(self):
        second = Window(SETTINGS, "settings-second")
        code = second.wait_exit(10)
        second.kill()
        focused = [l for l in second.lines() if "already open" in l]
        note(f"second open exit={code}: {focused[-1].split(': ', 1)[-1] if focused else '-'}")
        check(code == 0 and focused, "second open did not focus the existing window")
        check(self.settings.p.poll() is None, "first window died")
        check(len(lantern_displays()) == 1, f"displays {lantern_displays()}")
        check(len(status()["windows"]) == 1, "status window count != 1")
        check(len(kwin_on(SETTINGS, FIND, "lantern_count")) and
              "count=1" in kwin_on(SETTINGS, FIND, "lantern_count")[0], "KWin window count != 1")

    def resize_to(self, width, height):
        """KWin-resizes the Settings window to width x height logical (client area) and waits
        for the helper's Resized equal to the window's own physical request. Returns
        (request, resized, seconds from the KWin call, seconds from the request)."""
        w = self.settings
        assert w is not None
        t0 = time.time()
        scales = []
        for line in kwin_on(SETTINGS, RESIZE, "lantern_resize", WIDTH=width, HEIGHT=height):
            note(f"kwin: {line}")
            scales += re.findall(r"scale=([0-9.]+)$", line)
        check(len(scales) == 1, f"KWin reported no output scale for the window: {scales}")

        def answered():
            reqs = w.find("resize", after=t0)
            if not reqs:
                return None
            want = reqs[-1][1]
            for ts, got in w.find("resized", after=t0):
                if (got["w"], got["h"]) == (want["w"], want["h"]):
                    return reqs[-1], (ts, got)
            return None
        try:
            ((rts, req), (ats, got)), _ = wait_until("Resized matching the request", 3, answered,
                                                     interval=0.05)
        except Fail:
            note(f"requests: {w.find('resize', after=t0)} answers: {w.find('resized', after=t0)}")
            raise
        note(f"window requested {req['w']}x{req['h']} dpi={req['dpi']} "
             f"(logical {width}x{height}, scale {int(req['w']) / width:.4g}); "
             f"helper Resized {got['w']}x{got['h']} {ats - t0:.3f} s after the KWin resize "
             f"({ats - rts:.3f} s after the window's request); KWin output scale {scales[0]}")
        return req, got, ats - t0, float(scales[0])

    def resize_reflows(self):
        # Step away first: the window may already be 900x900 (its size is remembered), and a
        # no-op resize sends nothing.
        self.resize_to(1000, 700)
        req, got, secs, scale = self.resize_to(RESIZE_LOGICAL, RESIZE_LOGICAL)
        want = round(RESIZE_LOGICAL * scale)
        check(int(req["w"]) == int(req["h"]) == want,
              f"request {req['w']}x{req['h']} is not {RESIZE_LOGICAL} x KWin scale {scale} = {want}")
        check(secs <= 1.0, f"Resized took {secs:.3f} s (> 1 s)")

    def close_quits(self):
        w = self.settings
        assert w is not None
        for line in kwin_on(SETTINGS, CLOSE, "lantern_close"):
            note(f"kwin: {line}")
        code = w.wait_exit(5)
        w.kill()
        closed = w.find("closed")
        note(f"window exit={code}, closed {closed[-1][1] if closed else '-'}")
        check(code == 0, f"window exit {code}")
        _, secs = wait_until("lantern display gone", 5, lambda: not lantern_displays())
        try:
            _, tsecs = wait_until("settings task gone from other displays", 5,
                                  lambda: not tasks_off_display0(SETTINGS))
        except Fail as e:
            raise Fail(f"{e}: {tasks_off_display0(SETTINGS)}")
        note(f"lantern displays: [] after {secs:.1f} s; no settings task off display 0 after "
             f"{tsecs:.1f} s")
        check(status()["windows"] == [], "status still lists a window")

    def app_exit_closes_window(self):
        w = Window(JELLY, "jelly", debug_input="key:Escape;key:Escape")
        try:
            wait_until("jelly opened", 15, lambda: w.find("opened"))
            note(f"opened {w.find('opened')[0][1]}")
            code = w.wait_exit(15)
        finally:
            w.kill()
        steps = [l for l in w.lines() if ": debug input" in l]
        closed = w.find("closed")
        note(f"debug input steps sent: {len(steps)}; closed {closed[-1][1] if closed else '-'}; "
             f"window exit={code}")
        check(code == 0, f"window exit {code}")
        check(closed and closed[-1][1].get("reason") == "AppExited", "not closed by app exit")
        wait_until("lantern display gone", 5, lambda: not lantern_displays())
        wait_until("jelly task gone from other displays", 5, lambda: not tasks_off_display0(JELLY))

    def daemon_kill_restores_phone(self):
        stay_on_baseline()  # before the kill: reading it later would stop/start lanternd
        if KILL_SIGNAL == "KILL":
            return self.daemon_sigkill_recovers()
        try:
            systemctl("kill", UNIT)
            st, secs = wait_until("phone restored", 5,
                                  lambda: restored(s := phone_state()) and s, interval=0.2)
            note(f"restored {secs:.1f} s after kill: {st}")
            active = systemctl("is-active", UNIT, check_rc=False).strip()
            note(f"{UNIT} after kill: {active}; starting it again")
        finally:
            systemctl("start", UNIT)
        st, secs = wait_ready(15)
        note(f"ready again after {secs:.1f} s")

    def daemon_sigkill_recovers(self):
        """SIGKILL: the daemon cannot clean up. The helper and its guard still exit and restore
        the phone, systemd restarts the daemon (Restart=on-failure), and the new session start
        removes the dead daemon's adb forward."""
        before = phone_state()
        check(armed(before), f"phone not armed before the kill: {before}")
        old_fwd, old_pids = before["forwards"], set(before["helper_pids"])
        since = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        seen_restored = []
        try:
            systemctl("kill", "-s", "KILL", UNIT)

            def recovered():
                st = phone_state()
                if restored(st):
                    seen_restored.append(st)
                return (armed(st) and old_pids.isdisjoint(st["helper_pids"])
                        and st["forwards"] != old_fwd and st)
            st, secs = wait_until("new session armed without the old forward", 15, recovered,
                                  interval=0.2)
        finally:
            if systemctl("is-active", UNIT, check_rc=False).strip() != "active":
                systemctl("start", UNIT)
        note(f"before: forwards={old_fwd} helper_pids={sorted(old_pids)}")
        note(f"after {secs:.1f} s: {st}")
        note(f"restored state observed between the sessions: {'yes' if seen_restored else 'no'}")
        removed = [ANSI.sub("", l).split(": ", 1)[-1] for l in
                   sh("journalctl", "--user", "-u", UNIT, "--since", since, "--no-pager",
                      "-o", "cat").splitlines() if "stale lantern forward" in l]
        note(f"daemon log: {removed}")
        check(len(st["forwards"]) == 1 and st["forwards"][0] not in old_fwd,
              f"stale forward kept: {st['forwards']}")
        rst, rsecs = wait_ready(15)
        check(rst["serial"] == SERIAL, f"status {rst}")
        note(f"lanternd restarted by systemd; ready: {json.dumps(rst)}")

    def usb_yank(self):
        stay_on_baseline()
        before = status()
        pids = phone_state()["helper_pids"]
        check(before["state"] == "ready", f"not ready before the yank: {before}")
        input(">>> Unplug the phone's USB cable now, then press Enter ")
        wait_until("device gone", 20, lambda: SERIAL not in sh("adb", "devices"))
        st, _ = wait_until("daemon idle", 10, lambda: (s := status())["state"] == "idle" and s)
        note(f"unplugged: daemon {st['state']}")
        print(">>> Keep it unplugged for 10 s (watch the phone screen)...", flush=True)
        time.sleep(10)
        lit = input(">>> Is the phone's screen on now (panel restored)? [y/n] ").strip().lower()
        check(lit.startswith("y"), "panel not restored while unplugged")
        input(">>> Plug the phone back in, then press Enter ")
        sh("adb", "-s", SERIAL, "wait-for-device", timeout=120)
        log = adb("logcat", "-d", "-s", "lantern")
        restores = [l for l in log.splitlines() if "restored" in l]
        note(f"helper log: {restores[-1].strip() if restores else '(no restore line)'}")
        st, secs = wait_ready(15)
        new = phone_state()["helper_pids"]
        note(f"fresh session {secs:.1f} s after replug; helper pids {pids} -> {new}")
        check(set(new).isdisjoint(pids), "helper process was not replaced")
        check(any("stay_on_while_plugged_in to " + stay_on_baseline() in l for l in restores),
              "no stay-awake restore logged")

    # --- Plan 2 ----------------------------------------------------------------------------

    def companion_linked(self):
        """A fresh session links the companion: `lantern status` shows companion ready within
        10 s of the session being ready."""
        systemctl("restart", UNIT)
        st, secs = wait_ready(15)
        note(f"ready {secs:.1f} s after the restart: companion={st.get('companion')}")
        st, csecs = wait_until("companion ready", 10,
                               lambda: (s := status()).get("companion") == "ready" and s)
        note(f"companion ready {csecs:.1f} s after ready: {json.dumps(st)}")
        ph = phone_state()
        note(f"phone keyboard during the session: ime={ph['ime']} imes={ph['imes']}")

    def catalogue_tracks_phone(self):
        found = entry_for(SERIAL, SETTINGS)
        check(found is not None, f"no menu entry of {SETTINGS} for {SERIAL} in {applications_dir()}")
        assert found is not None
        path, settings = found
        icon = Path(settings.get("Icon", ""))
        note(f"{path.name}: Name={settings.get('Name')} Icon={icon} "
             f"Exec={settings.get('Exec')}")
        check(settings.get("Name") == "Settings", f"Name={settings.get('Name')!r}")
        check(icon.is_absolute() and icon.is_file()
              and icon.read_bytes()[:8] == b"\x89PNG\r\n\x1a\n", f"Icon {icon} is not a PNG")
        def comp():
            return entry_for(SERIAL, COMPANION)
        if comp() is not None or test_activity_listed():
            note("test screen left enabled by an earlier run; disabling it first")
            test_activity(False)
            wait_until("companion entry gone before the test", 10,
                       lambda: comp() is None, interval=0.2)
        try:
            test_activity(True)
            (path, entry), secs = wait_until("companion entry appears", 5, comp, interval=0.1)
            note(f"TEST_ACTIVITY on: {path.name} appeared after {secs:.1f} s "
                 f"(Name={entry.get('Name')})")
            test_activity(False)
            _, secs = wait_until("companion entry removed", 5,
                                 lambda: comp() is None, interval=0.1)
            note(f"TEST_ACTIVITY off: {path.name} removed after {secs:.1f} s")
        finally:
            test_activity(False)

    def title_is_label(self):
        w = Window(SETTINGS, "settings-title")
        try:
            wait_until("opened", 15, lambda: w.find("opened"))
            desc, _ = wait_until("KWin lists the window", 5,
                                 lambda: (d := kwin_on(SETTINGS, DESCRIBE, "lantern_describe"))
                                 and "count=1" in d[0] and d, interval=0.3)
            for line in desc:
                note(f"kwin: {line}")
            m = re.search(r" caption=(.*) client=", desc[1])
            check(m and m.group(1) == "Settings", f"caption {m.group(1) if m else desc[1]!r}")
            sizes = windows_json()
            before = sizes.stat().st_mtime_ns if sizes.exists() else None
            kwin_on(SETTINGS, CLOSE, "lantern_close")
            code = w.wait_exit(5)
            check(code == 0, f"window exit {code}")
            # The size is remembered under SERIAL's key (the window resolved SERIAL).
            key = size_key_of(SETTINGS)
            after = sizes.stat().st_mtime_ns if sizes.exists() else None
            check(after is not None and after != before, f"{sizes} was not written on close")
            keys = json.loads(sizes.read_text())
            check(key in keys, f"{sizes} has no key {key!r}: {sorted(keys)}")
            note(f"windows.json[{key!r}] = {keys[key]}")
        finally:
            w.kill()
        wait_until("lantern display gone", 5, lambda: not lantern_displays())

    def _test_window(self, tag, steps, timeout):
        """Opens the companion's test screen with debug steps (ending in `close`), waits for the
        window to exit, and returns (window, phone time before the open)."""
        test_activity(True)
        wait_until("test screen in the launcher", 5, test_activity_listed, interval=0.2)
        since = phone_now()
        w = Window(COMPANION, tag, debug_input=("focus;" if NESTED else "") + steps)
        try:
            wait_until("test screen opened", 15, lambda: w.find("opened"))
            note(f"opened {w.find('opened')[0][1]}")
            code = w.wait_exit(timeout)
        finally:
            w.kill()
        closed = w.find("closed")
        note(f"window exit={code}, closed {closed[-1][1] if closed else '-'}")
        check(code == 0, f"window exit {code}")
        return w, since

    def ime_unicode_text(self):
        ok = False
        try:
            ime = phone_state()["ime"]
            note(f"default_input_method during the session: {ime}")
            check(ime == LANTERN_IME, f"the Lantern keyboard is not selected: {ime}")
            _, since = self._test_window(
                "ime-text", "tap:0.5,0.15;text:Tiếng Việt 👍;compose:việ;finish;close", 40)
            texts = [m for _, m in logcat_since(since, ["LanternTest"])]
            for t in texts:
                note(f"LanternTest: {t}")
            check(texts and texts[-1] == f"text={IME_TEXT}",
                  f"last LanternTest line {texts[-1] if texts else None!r}, want 'text={IME_TEXT}'")
            ops = [m for _, m in logcat_since(since, ["LanternCompanion"]) if "ime op=" in m]
            note(f"companion: {ops}")
            leaked = [m for _, m in logcat_since(since, ["LanternCompanion", "lantern"])
                      if "Tiếng" in m or "việ" in m]
            check(not leaked, f"typed text in the companion/helper log: {leaked}")
            ok = True
        finally:
            cleanup(ok, ("disable the test screen", lambda: test_activity(False)))

    def clipboard_both_ways(self):
        saved = klipper("getClipboardContents")
        saved = saved[:-1] if saved.endswith("\n") else saved
        note(f"saved the PC clipboard text ({len(saved)} chars)")
        tag = f"{os.getpid()}{int(time.time()) % 100000}"
        pc_text, phone_text = f"lantern-pc-{tag}", f"from-phone-{tag}"
        ok = False
        try:
            # 1. PC -> phone, before the window opens (the helper logs the length only).
            since = phone_now()
            klipper("setClipboardContents", pc_text)
            want = f"clip: PC clipboard ({len(pc_text)} chars) set on the phone"
            _, secs = wait_until("PC clipboard set on the phone", 5, lambda: [
                m for _, m in logcat_since(since, ["lantern"]) if m == want], interval=0.3)
            note(f"helper: {want!r} within {secs:.1f} s")
            # 2. Paste it into the test screen; 3. copy a phone text back to the PC.
            test_activity(True)
            wait_until("test screen in the launcher", 5, test_activity_listed, interval=0.2)
            since = phone_now()
            w = Window(COMPANION, "clipboard", debug_input=("focus;" if NESTED else "") +
                       f"tap:0.5,0.15;key:Ctrl+V;key:Ctrl+A;text:{phone_text};key:Ctrl+A;"
                       f"key:Ctrl+C;close")
            try:
                wait_until("test screen opened", 15, lambda: w.find("opened"))
                _, _ = wait_until("Klipper holds the phone's copy", 40,
                                  lambda: klipper("getClipboardContents").rstrip("\n")
                                  == phone_text, interval=0.05)
                got_at = time.time()
                code = w.wait_exit(15)
            finally:
                w.kill()
            check(code == 0, f"window exit {code}")
            ctrl_c = [datetime.fromisoformat(l.split()[0].replace("Z", "+00:00")).timestamp()
                      for l in w.lines() if "debug input" in l and "Key(KeyC, true)" in l]
            check(ctrl_c, "no Ctrl+C debug step in the window log")
            lag = got_at - ctrl_c[-1]
            note(f"phone -> PC: Klipper held {phone_text!r} {lag:.3f} s after the Ctrl+C step")
            check(lag <= 2.0, f"phone copy reached Klipper after {lag:.3f} s (> 2 s)")
            texts = [m for _, m in logcat_since(since, ["LanternTest"])]
            for t in texts:
                note(f"LanternTest: {t}")
            check(f"text={pc_text}" in texts, f"Ctrl+V did not paste {pc_text!r}")
            helper = [m for _, m in logcat_since(since, ["lantern"]) if m.startswith("clip:")]
            note(f"helper: {helper}")
            check(helper.count(f"clip: phone copy ({len(phone_text)} chars) sent to the PC") == 1,
                  "phone copy not sent exactly once")
            ok = True
        finally:
            # The user's clipboard first: disabling the test screen fails exactly when the phone
            # dropped, and the suite's own finally disables it again anyway.
            def restore():
                klipper("setClipboardContents", saved)
                note("restored the PC clipboard text")
            cleanup(ok, ("restore the PC clipboard", restore),
                    ("disable the test screen", lambda: test_activity(False)))

    def usb_yank_restores_keyboard(self):
        """Unplug: the phone's own cleanup puts the keyboard back. Checked after replug with
        lanternd stopped, so no new session switches it again first."""
        base = baseline()
        st = status()
        check(st["state"] == "ready", f"not ready before the yank: {st}")
        check(phone_state()["ime"] == LANTERN_IME, "the Lantern keyboard is not selected")
        since = phone_now()
        input(">>> Unplug the phone's USB cable now, then press Enter ")
        wait_until("device gone", 20, lambda: SERIAL not in sh("adb", "devices"))
        print(">>> Keep it unplugged for 10 s...", flush=True)
        time.sleep(10)
        systemctl("stop", UNIT)
        try:
            input(">>> Plug the phone back in, then press Enter ")
            sh("adb", "-s", SERIAL, "wait-for-device", timeout=120)
            ph = phone_state()
            note(f"after replug, lanternd stopped: {ph}")
            lines = [m for _, m in logcat_since(since, ["lantern", "LanternCompanion"])
                     if "ime_restored" in m or "default_input_method" in m]
            for l in lines:
                note(f"logcat: {l}")
            check(any("ime_restored" in l or f"default_input_method to {base['ime']}" in l
                      for l in lines), "no keyboard restore logged while unplugged")
            check(ph["ime"] == base["ime"] and ph["imes"] == base["imes"],
                  f"keyboard not restored: {ph['ime']} / {ph['imes']}, want {base}")
            check(ph["stay_on"] == base["stay_on"], f"stay_on {ph['stay_on']}")
        finally:
            systemctl("start", UNIT)
        wait_ready(15)

    # --- Plan 3 ----------------------------------------------------------------------------

    def phones_keep_their_entries(self):
        """Another phone's per-phone menu entry survives this phone's full sync byte for byte,
        and this phone's own entry names it with --phone."""
        # The format of catalog::desktop_file for a non-primary phone "lantern-test-other":
        # lantern.dev.lantern.test.other-plantern_test_other.desktop.
        name, text = per_phone_entry("dev.lantern.test.other", "lantern-test-other",
                                     "Lantern test other")
        other = applications_dir() / name
        check(not other.exists(), f"{other} exists before the test")
        planted = text.encode()
        ok = False
        try:
            other.parent.mkdir(parents=True, exist_ok=True)
            other.write_bytes(planted)
            systemctl("restart", UNIT)
            _, secs = wait_ready(15)
            _, csecs = wait_until("companion ready", 10,
                                  lambda: status().get("companion") == "ready")
            note(f"ready {secs:.1f} s, companion ready {csecs:.1f} s after; "
                 "waiting 10 s for the full sync")
            time.sleep(10)
            check(other.exists(), f"{other.name} was deleted by {SERIAL}'s full sync")
            check(other.read_bytes() == planted, f"{other.name} was rewritten")
            note(f"{other.name} unchanged")
            found = entry_for(SERIAL, SETTINGS)
            check(found is not None, f"no menu entry of {SETTINGS} for {SERIAL}")
            assert found is not None
            path, settings = found
            note(f"{path.name}: X-Lantern-Serial={settings.get('X-Lantern-Serial')} "
                 f"Exec={settings.get('Exec')}")
            check(f"--phone {SERIAL} " in settings.get("Exec", ""),
                  f"Exec={settings.get('Exec')!r} lacks --phone {SERIAL}")
            ok = True
        finally:
            cleanup(ok, ("remove the planted entry", lambda: other.unlink(missing_ok=True)))

    def not_on_this_phone_notice(self):
        """After this phone's full sync, opening an app only another phone has is refused
        (exit 1) with "Connect your <that phone's name>", and this phone's device record has
        its name. The window resolves the other phone (it has the app; the live one lacks it) and
        logs that line both when lanternd offers to connect it and on the window's own
        fallback notice."""
        pkg = "dev.lantern.test.absent"
        name, planted = per_phone_entry(pkg, "lantern-test-other", "Lantern test absent")
        entry = applications_dir() / name
        record = devices_dir() / "lantern-test-other.json"
        check(not entry.exists(), f"{entry} exists before the test")
        check(not record.exists(), f"{record} exists before the test")
        ok = False
        try:
            entry.parent.mkdir(parents=True, exist_ok=True)
            entry.write_text(planted)
            record.parent.mkdir(parents=True, exist_ok=True)
            record.write_text(json.dumps({"companion": True, "since": 0, "name": "Test Phone"}))
            wait_ready(15)
            _, csecs = wait_until("companion ready", 10,
                                  lambda: status().get("companion") == "ready")
            note(f"companion ready after {csecs:.1f} s; waiting 10 s for the full sync")
            time.sleep(10)
            t0 = time.monotonic()
            w = Window(pkg, "not_on_this_phone")
            code = w.wait_exit(5)
            secs = time.monotonic() - t0
            lines = w.lines()
            for line in lines:
                if "connect notice" in line or "not on this phone" in line:
                    note(f"stderr: {line.strip()}")
            check(code == 1, f"lantern open {pkg} -> {code}, want 1 ({w.log})")
            check(any("connect notice: Connect your Test Phone" in l for l in lines),
                  f"no 'connect notice: Connect your Test Phone' in {w.log}")
            note(f"refused in {secs:.1f} s")
            mine = devices_dir() / f"{SERIAL}.json"
            check(mine.exists(), f"{mine} is missing")
            name = json.loads(mine.read_text()).get("name")
            check(isinstance(name, str) and name.strip(), f"{mine.name} has no name: {name!r}")
            note(f"{mine.name}: name={name!r}")
            ok = True
        finally:
            cleanup(ok, ("remove the planted entry", lambda: entry.unlink(missing_ok=True)),
                    ("remove the planted device record", lambda: record.unlink(missing_ok=True)))

    def keyboard_setting_off(self):
        """`lantern_keyboard = false` gives a stay-awake-only session at the next connection:
        the phone's keyboard settings stay its own, the companion is still ready. Back on, the
        Lantern keyboard is selected again; and with lanternd stopped (item 7's path) all three
        settings are the originals."""
        base = baseline()  # the originals, read with lanternd stopped
        note(f"originals: default_input_method={base['ime']} "
             f"enabled_input_methods={base['imes']} stay_on_while_plugged_in={base['stay_on']}")
        path = settings_file()
        saved_file = path.read_bytes() if path.exists() else None
        note(f"{path}: " + (f"saved ({len(saved_file)} bytes)" if saved_file is not None
                            else "absent"))

        def restore_file():
            if saved_file is None:
                path.unlink(missing_ok=True)
                note(f"removed {path} (it was absent)")
            else:
                path.write_bytes(saved_file)
                note(f"restored {path}")

        def restart_ready(what):
            systemctl("restart", UNIT)
            _, secs = wait_ready(15)
            st, csecs = wait_until("companion ready", 10,
                                   lambda: (s := status()).get("companion") == "ready" and s)
            note(f"{what}: ready {secs:.1f} s after the restart, companion ready {csecs:.1f} s "
                 f"after: {json.dumps(st)}")

        ok = False
        try:
            # 2. Every other line kept; lantern_keyboard replaced.
            old = saved_file.decode("utf-8", "replace") if saved_file is not None else ""
            lines = [l for l in old.splitlines()
                     if not l.strip().startswith("lantern_keyboard")]
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            path.write_text("\n".join(lines + ["lantern_keyboard = false"]) + "\n")
            since = phone_now()
            restart_ready("keyboard off")
            # 3. The phone's keyboard is untouched; only stay-awake is Lantern's.
            ph = phone_state()
            note(f"during the session: ime={ph['ime']} imes={ph['imes']} stay_on={ph['stay_on']}")
            check(ph["ime"] == base["ime"], f"default_input_method {ph['ime']}, want {base['ime']}")
            check(ph["imes"] == base["imes"],
                  f"enabled_input_methods {ph['imes']}, want {base['imes']}")
            check(ph["stay_on"] == STAY_ON_SESSION, f"stay_on {ph['stay_on']}")
            off, _ = wait_until("'keyboard off in Lantern settings' in logcat", 5, lambda: [
                m for _, m in logcat_since(since, ["lantern"])
                if "keyboard off in Lantern settings" in m], interval=0.3)
            note(f"helper: {off[-1]}")
            # 4. Back on: the next session selects the Lantern keyboard again.
            restore_file()
            restart_ready("keyboard back on")
            ime, secs = wait_until("the Lantern keyboard selected", 10,
                                   lambda: (i := phone_state()["ime"]) == LANTERN_IME and i,
                                   interval=0.3)
            note(f"default_input_method {ime} {secs:.1f} s after ready")
            ok = True
        finally:
            # The settings file first, then 5. the phone's settings with lanternd stopped.
            cleanup(ok, ("restore settings.toml", restore_file),
                    ("phone settings are the originals with lanternd stopped",
                     lambda: self._keyboard_originals_after_stop(base)))

    def _keyboard_originals_after_stop(self, base):
        """Stops lanternd as item 7 does (`systemctl kill`, wait for the phone to be released),
        checks the three settings equal `base`, restores any that don't by hand (and fails),
        then starts lanternd again."""
        keys = [("secure", "default_input_method", "ime"),
                ("secure", "enabled_input_methods", "imes"),
                ("global", "stay_on_while_plugged_in", "stay_on")]
        try:
            systemctl("kill", UNIT)
            st, secs = wait_until("phone released after kill", 10,
                                  lambda: released(s := phone_state()) and not s["journal"] and s,
                                  interval=0.2)
            note(f"released {secs:.1f} s after kill: ime={st['ime']} imes={st['imes']} "
                 f"stay_on={st['stay_on']}")
            wrong = [(ns, key, st[f], base[f]) for ns, key, f in keys if st[f] != base[f]]
            for ns, key, got, want in wrong:
                if want in (None, "null"):
                    adb("shell", f"settings delete {ns} {key}")
                else:
                    adb("shell", f"settings put {ns} {key} {shlex.quote(want)}")
                note(f"restored {ns}/{key} by hand: {got} -> {want}")
            check(not wrong, "not the originals with lanternd stopped: " +
                  ", ".join(f"{key}={got} (want {want})" for _, key, got, want in wrong))
        finally:
            if systemctl("is-active", UNIT, check_rc=False).strip() != "active":
                systemctl("start", UNIT)
        _, secs = wait_ready(15)
        note(f"ready again after {secs:.1f} s")

    def clipboard_setting_off(self):
        """A hand edit `clipboard_sync = false` in settings.toml applies live (within 2 s): a
        phone copy made through the test screen never reaches Klipper."""
        path = settings_file()
        saved_file = path.read_bytes() if path.exists() else None
        note(f"{path}: " + (f"saved ({len(saved_file)} bytes)" if saved_file is not None
                            else "absent"))
        saved = klipper("getClipboardContents")
        saved = saved[:-1] if saved.endswith("\n") else saved
        note(f"saved the PC clipboard text ({len(saved)} chars)")
        phone_text = f"phone-off-{os.getpid()}{int(time.time()) % 100000}"
        ok = False
        try:
            # 2. Every other line kept; clipboard_sync replaced.
            old = saved_file.decode("utf-8", "replace") if saved_file is not None else ""
            lines = [l for l in old.splitlines()
                     if not l.strip().startswith("clipboard_sync")]
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            path.write_text("\n".join(lines + ["clipboard_sync = false"]) + "\n")
            time.sleep(3)
            # 3. A phone copy through the test screen (item 13's path).
            test_activity(True)
            wait_until("test screen in the launcher", 5, test_activity_listed, interval=0.2)
            since = phone_now()
            w = Window(COMPANION, "clipboard_off", debug_input=("focus;" if NESTED else "") +
                       f"tap:0.5,0.15;key:Ctrl+A;text:{phone_text};key:Ctrl+A;key:Ctrl+C;close")
            try:
                wait_until("test screen opened", 15, lambda: w.find("opened"))
                code = w.wait_exit(15)
            finally:
                w.kill()
            check(code == 0, f"window exit {code}")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline:
                check(klipper("getClipboardContents").rstrip("\n") != phone_text,
                      "Klipper got the phone's copy with clipboard_sync = false")
                time.sleep(0.1)
            note("Klipper did not get the phone's copy within 3 s")
            texts = [m for _, m in logcat_since(since, ["LanternTest"])]
            check(f"text={phone_text}" in texts, f"the test screen never held {phone_text!r}")
            # The helper cannot know the PC's setting (no message tells it), so it still sends
            # the copy over the link and lanternd drops it; its line is recorded, not checked.
            helper = [m for _, m in logcat_since(since, ["lantern"]) if m.startswith("clip:")]
            note(f"helper: {helper}")
            ok = True
        finally:
            # The settings file first (so sync is back on), then the user's clipboard text,
            # then the test screen; each guarded on its own.
            def restore_file():
                if saved_file is None:
                    path.unlink(missing_ok=True)
                    note(f"removed {path} (it was absent)")
                else:
                    path.write_bytes(saved_file)
                    note(f"restored {path}")

            def restore_clipboard():
                klipper("setClipboardContents", saved)
                note("restored the PC clipboard text")
            cleanup(ok, ("restore settings.toml", restore_file),
                    ("restore the PC clipboard", restore_clipboard),
                    ("disable the test screen", lambda: test_activity(False)))


    def disconnect_and_connect(self):
        """`lantern disconnect SERIAL` ends the session and the helper restores the phone within
        5 s; lanternd then stays `paused` with the phone `off` (in `status.phones` and its
        record) and no restart for 15 s. `lantern connect SERIAL` makes it ready again within
        15 s and clears `off`."""
        base = baseline()  # 1. the originals, read with lanternd stopped
        note(f"originals: stay_on={base['stay_on']} ime={base['ime']} imes={base['imes']}")
        connected = False
        try:
            # 2. Disconnect: restored, and paused (off) for 15 s.
            note(f"lantern disconnect {SERIAL}: {lantern_cmd('disconnect', SERIAL)}")
            st, secs = wait_until("phone restored", 5,
                                  lambda: restored(s := phone_state()) and s, interval=0.2)
            note(f"restored {secs:.1f} s after disconnect: {st}")
            check(device_record().get("off") is True,
                  f"record not off after disconnect: {device_record()}")
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                now = status()
                check(now["state"] == "paused" and now["serial"] == SERIAL,
                      f"not paused after disconnect: {json.dumps(now)}")
                check(phone_status(SERIAL).get("state") == "off",
                      f"phone not off after disconnect: {json.dumps(now)}")
                check(not phone_state()["helper_pids"], "a helper started while paused")
                time.sleep(1)
            note(f"paused for 15 s: {json.dumps(status())}")
            # 3. Connect: ready again, off cleared.
            note(f"lantern connect {SERIAL}: {lantern_cmd('connect', SERIAL)}")
            _, secs = wait_ready(15)
            connected = True
            note(f"ready {secs:.1f} s after connect")
            check("off" not in phone_status(SERIAL),
                  f"status still says off: {json.dumps(phone_status(SERIAL))}")
            check(device_record().get("off") is not True,
                  f"record still off after connect: {device_record()}")
        finally:
            # 4. Leave lanternd ready, whatever failed.
            if not connected:
                cleanup(False, ("lantern connect", lambda: lantern_cmd("connect", SERIAL)),
                        ("lanternd ready", lambda: wait_ready(15)))


    def tray_menu(self):
        """The tray (real session bus, desktop unlocked): lanternd's StatusNotifierItem is
        registered with the watcher, its menu has a checked row named after the phone (id
        100n), `Quit Lantern` (11) and the Autoconnect row (14). Clicking the phone row switches
        it off (state `off`) and restores the phone within 5 s and unchecks the row; clicking
        it again makes it ready within 15 s. Opening the menu by hand stays a manual check; a
        screenshot is saved for it."""
        # baseline() restarts lanternd, which gives the tray a new bus name: take it first.
        base = baseline()
        note(f"originals: stay_on={base['stay_on']} ime={base['ime']} imes={base['imes']}")
        wait_ready(15)
        name = tray_name()

        def registered():
            out = sh("busctl", "--user", "--json=short", "get-property",
                     "org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher",
                     "org.kde.StatusNotifierWatcher", "RegisteredStatusNotifierItems")
            return [i for i in json.loads(out)["data"] if i.startswith(name)]
        items, secs = wait_until("lantern's tray item registered", 10, registered)
        note(f"registered {items} ({secs:.1f} s)")

        def rows():
            return tray_rows(name)

        def click(cid):
            tray_click(name, cid)

        device = adb("shell", "settings get global device_name").strip()
        (row, menu), _ = wait_until(f"a checkmark row {device!r}", 15,
                                    lambda: tray_phone_row(name, device))
        note(f"menu: {menu}; the phone's row is {row}")
        check(menu[row][1] == 1, f"row {row} is not checked: {menu[row]}")
        check(menu.get(11, ("",))[0] == "Quit Lantern", f"no Quit Lantern row: {menu}")
        check(14 in menu, f"no Autoconnect row (14): {menu}")
        connected = False
        try:
            click(row)
            st, secs = wait_until("off after the tray click", 5,
                                  lambda: (p := phone_status(SERIAL)).get("state") == "off"
                                  and p)
            note(f"off {secs:.1f} s after the click: {json.dumps(st)}")
            st, secs = wait_until("phone restored", 5,
                                  lambda: restored(s := phone_state()) and s, interval=0.2)
            note(f"restored {secs:.1f} s later")
            wait_until(f"row {row} unchecked", 5,
                       lambda: rows().get(row, ("", None))[1] == 0)
            click(row)
            _, secs = wait_ready(15)
            connected = True
            note(f"ready {secs:.1f} s after the tray's second click")
        finally:
            if not connected:
                cleanup(False, ("lantern connect", lambda: lantern_cmd("connect", SERIAL)),
                        ("lanternd ready", lambda: wait_ready(15)))
        shot = LOG_DIR / "tray.png"
        r = subprocess.run(["spectacle", "-b", "-n", "-o", str(shot)], capture_output=True,
                           text=True, timeout=30)
        note(f"screenshot for the report: {shot}" if r.returncode == 0 and shot.exists()
             else f"no screenshot (spectacle -> {r.returncode}: {r.stderr.strip()})")


    def unlock_prompt(self):
        """Manual, PIN-locked phone (Motorola ZD222LMDFV: run with LANTERN_SERIAL=ZD222LMDFV and
        lanternd serving it). Sahaj types the PIN into the real KDE dialog; this item never sees
        or prints it. Locks the phone, copies a tag on the PC and expects "Unlock your <name>"
        (a Notify on the session bus) within 3 s; after Sahaj submits the right PIN the phone
        unlocks within 5 s, the tag is on the phone's clipboard (pasted into the test screen)
        and the panel is off again. Then a wrong-PIN pass: "That PIN didn't unlock", exactly
        one `unlock: still locked` in the helper log, the panel off, and Sahaj unlocks through
        "Try again". The PC clipboard and the test screen are restored afterwards."""
        check(SERIAL == "ZD222LMDFV",
              f"item 21 needs the PIN-locked Motorola: LANTERN_SERIAL=ZD222LMDFV (got {SERIAL}), "
              "and `systemctl --user set-environment LANTERN_SERIAL=ZD222LMDFV` + restart lanternd")
        wait_ready(15)
        st = status()
        check(st.get("serial") == SERIAL, f"lanternd serves {st.get('serial')}, not {SERIAL}")
        name = adb("shell", "settings", "get", "global", "device_name").strip() or SERIAL
        note(f"phone name: {name}")

        def locked():
            for line in adb("shell", "dumpsys", "trust").splitlines():
                if "(current)" in line:
                    m = re.search(r"deviceLocked=([01])\b", line)
                    return m.group(1) == "1" if m else None
            return None

        def lock():
            adb("shell", "input", "keyevent", "KEYCODE_SLEEP")
            _, secs = wait_until("deviceLocked=1", 15, lambda: locked() is True, interval=0.3)
            note(f"phone locked {secs:.1f} s after KEYCODE_SLEEP")

        # Notify calls on the real session bus. Only matched summaries are ever printed.
        mon_log = LOG_DIR / "notifications.log"
        mon_f = open(mon_log, "w")
        monitor = subprocess.Popen(["busctl", "--user", "monitor", "org.freedesktop.Notifications"],
                                   stdout=mon_f, stderr=subprocess.STDOUT)

        def summaries():
            text = mon_log.read_text(errors="replace")
            return re.findall(r'STRING "((?:Unlock your|That PIN didn\'t unlock|Couldn\'t unlock) '
                              r'[^"]*)";', text)

        def notified(prefix, before):
            got = [x for x in summaries()[before:] if x.startswith(prefix)]
            return got

        def ask(msg):
            print(f"    >>> {msg}", flush=True)

        saved = klipper("getClipboardContents")
        saved = saved[:-1] if saved.endswith("\n") else saved
        note(f"saved the PC clipboard text ({len(saved)} chars)")
        tag = f"lantern-unlock-{os.getpid()}{int(time.time()) % 100000}"
        ok = False
        try:
            time.sleep(1)  # the monitor is subscribed
            # 1. Right PIN.
            lock()
            before = len(summaries())
            klipper("setClipboardContents", tag)
            copied = time.monotonic()
            got, _ = wait_until("'Unlock your' notification", 10,
                                lambda: notified("Unlock your", before), interval=0.1)
            lag = time.monotonic() - copied
            note(f"Notify {got[0]!r} {lag:.1f} s after the copy")
            check(got[0] == f"Unlock your {name}", f"summary {got[0]!r}")
            check(lag <= 3.0, f"notification after {lag:.1f} s (> 3 s)")
            ask(f"Click 'Enter PIN…' in the notification and type {name}'s PIN.")
            journal_since = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
            _, secs = wait_until("deviceLocked=0 after the PIN", 180, lambda: locked() is False,
                                 interval=0.2)
            unlocked_at = time.time()
            sent = [l for l in sh("journalctl", "--user", "-u", UNIT, "--since", journal_since,
                                  "-o", "short-unix", "--no-pager").splitlines()
                    if "unlock sent to the helper" in l]
            check(sent, "no 'unlock sent to the helper' in lanternd's journal")
            took = unlocked_at - float(sent[-1].split()[0])
            note(f"deviceLocked=0 {took:.1f} s after lanternd sent the unlock")
            check(took <= 5.0, f"unlocked {took:.1f} s after the unlock was sent (> 5 s)")
            wait_until("panel off again", 5,
                                  lambda: phone_state()["panel"] == "Off", interval=0.3)
            note("panel off again after the unlock")
            _, since = self._test_window("unlock-paste", "tap:0.5,0.15;key:Ctrl+V;close", 40)
            texts = [m for _, m in logcat_since(since, ["LanternTest"])]
            for t in texts:
                note(f"LanternTest: {t}")
            check(f"text={tag}" in texts, "the PC copy made while locked is not on the phone")
            # 2. Wrong PIN, then the right one through "Try again".
            lock()
            since = phone_now()
            before = len(summaries())
            klipper("setClipboardContents", tag + "-2")
            wait_until("'Unlock your' notification (second stretch)", 10,
                       lambda: notified("Unlock your", before), interval=0.1)
            ask("Click 'Enter PIN…' and type a WRONG PIN.")
            got, _ = wait_until("'That PIN didn't unlock'", 180,
                                lambda: notified("That PIN didn't unlock", before), interval=0.2)
            note(f"Notify {got[0]!r}")
            check(got[0] == f"That PIN didn't unlock {name}", f"summary {got[0]!r}")
            check(locked() is True, "the phone unlocked with a wrong PIN?")
            still = [m for _, m in logcat_since(since, ["lantern"]) if m == "unlock: still locked"]
            check(len(still) == 1, f"{len(still)} 'unlock: still locked' lines, want 1")
            wait_until("panel off after the wrong PIN", 5,
                       lambda: phone_state()["panel"] == "Off", interval=0.3)
            note("panel off after the wrong PIN")
            ask("Click 'Try again' and type the right PIN.")
            wait_until("deviceLocked=0 after 'Try again'", 180, lambda: locked() is False,
                       interval=0.2)
            note("unlocked through 'Try again'")
            ok = True
        finally:
            def restore():
                klipper("setClipboardContents", saved)
                note("restored the PC clipboard text")

            def stop_monitor():
                monitor.terminate()
                try:
                    monitor.wait(5)
                except subprocess.TimeoutExpired:
                    monitor.kill()
                mon_f.close()
            cleanup(ok, ("restore the PC clipboard", restore),
                    ("stop the notification monitor", stop_monitor),
                    ("disable the test screen", lambda: test_activity(False)))

    def swipe_phone_never_prompts(self):
        """A phone without a PIN (the Xiaomi: swipe or no lock screen) never gets the unlock
        prompt. Copies on the PC and on the phone as item 13 does, then checks: no `lock: phone
        locked` line in the helper log, no "phone locked" in lanternd's journal, the tray menu
        (lanternd's view) not locked (no row label starting with "Unlock", none equal to
        "Locked"), and no Notify
        titled "Unlock ..." on the session bus during the copies and 5 s after."""
        check(SERIAL != "ZD222LMDFV",
              "item 22 is for a phone without a PIN (the Xiaomi), not the PIN-locked Motorola")
        wait_ready(15)
        st = status()
        check(st.get("serial") == SERIAL, f"lanternd serves {st.get('serial')}, not {SERIAL}")
        since = phone_now()
        journal_since = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        # Notify calls on the real session bus. Only matched summaries are ever printed.
        mon_log = LOG_DIR / "notifications-22.log"
        mon_f = open(mon_log, "w")
        monitor = subprocess.Popen(["busctl", "--user", "monitor", "org.freedesktop.Notifications"],
                                   stdout=mon_f, stderr=subprocess.STDOUT)
        try:
            time.sleep(1)  # the monitor is subscribed
            # 1. Copy on the PC and on the phone (item 13's path; it restores the clipboard).
            self.clipboard_both_ways()
            time.sleep(5)
        finally:
            monitor.terminate()
            try:
                monitor.wait(5)
            except subprocess.TimeoutExpired:
                monitor.kill()
            mon_f.close()
        # 2. Nothing saw a lock.
        locked_lines = [m for _, m in logcat_since(since, ["lantern"]) if m.startswith("lock:")]
        check(not locked_lines, f"helper lock lines on a swipe phone: {locked_lines}")
        journal = [l for l in sh("journalctl", "--user", "-u", UNIT, "--since", journal_since,
                                 "--no-pager").splitlines() if "phone locked" in l]
        check(not journal, f"lanternd logged a lock: {journal}")
        unlock_notes = re.findall(r'STRING "((?:Unlock your|That PIN didn\'t unlock|'
                                  r'Couldn\'t unlock) [^"]*)";',
                                  mon_log.read_text(errors="replace"))
        check(not unlock_notes, f"unlock notifications on a swipe phone: {unlock_notes}")
        note("no lock line, no 'phone locked', no Unlock notification")
        # 3. lanternd's view: the tray (its bus name is taken now, after any restart).
        pid = systemctl("show", "-p", "MainPID", "--value", UNIT).strip()
        check(pid not in ("", "0"), f"{UNIT} is not running")
        out = sh("busctl", "--user", "--json=short", "call", f"org.kde.StatusNotifierItem-{pid}-1",
                 "/MenuBar", "com.canonical.dbusmenu", "GetLayout", "iias", "--", "0", "-1", "0")
        _, (_, _, children) = json.loads(out)["data"]
        rows = {c["data"][0]: c["data"][1].get("label", {}).get("data", "") for c in children}
        note(f"tray rows: {rows}")
        check(not any(l.startswith("Unlock") for l in rows.values()),
              f"the tray shows an Unlock row: {rows}")
        check("Locked" not in rows.values(), f"the tray says locked: {rows}")

    # --- Plan 4b ---------------------------------------------------------------------------

    def per_phone_folders(self):
        """The menu has a folder for this phone, named after it; this phone's entries open
        with --phone; no entry is a shared (X-Lantern-Serials) one any more; a foreign
        lantern.*.desktop survives a restart and a full sync byte for byte."""
        foreign = applications_dir() / "lantern.dev.lantern.test.foreign.desktop"
        check(not foreign.exists(), f"{foreign} exists before the test")
        planted = ("[Desktop Entry]\nType=Application\nName=Not Lantern's\nExec=true\n"
                   "X-Lantern-Package=dev.lantern.test.foreign\nX-Lantern-User=0\n").encode()
        ok = False
        try:
            # 1. Someone else's file with a Lantern-like name (no X-Lantern-Serial=).
            foreign.parent.mkdir(parents=True, exist_ok=True)
            foreign.write_bytes(planted)
            # 2. A fresh lanternd and a full sync.
            systemctl("restart", UNIT)
            _, secs = wait_ready(15)
            _, csecs = wait_until("companion ready", 10,
                                  lambda: status().get("companion") == "ready")
            note(f"ready {secs:.1f} s, companion ready {csecs:.1f} s after; "
                 "waiting 10 s for the full sync")
            time.sleep(10)
            t = tag(SERIAL)
            # 3. This phone's submenu.
            menu = menus_dir() / "lantern.menu"
            check(menu.exists(), f"{menu} is missing")
            text = menu.read_text(errors="replace")
            check(f"<Category>X-Lantern-{t}</Category>" in text,
                  f"{menu.name} has no submenu for X-Lantern-{t}")
            # 4. Its folder, named after the phone.
            device = adb("shell", "settings get global device_name").strip()
            folder = directories_dir() / f"lantern-phone-{t}.directory"
            check(folder.exists(), f"{folder} is missing")
            keys = read_keys(folder)
            note(f"{folder.name}: Name={keys.get('Name')!r} (device_name {device!r})")
            check(keys.get("Name") == device, f"folder Name={keys.get('Name')!r}, want {device!r}")
            check(keys.get("X-Lantern-Serial") == SERIAL,
                  f"folder X-Lantern-Serial={keys.get('X-Lantern-Serial')!r}")
            # 5. This phone's Settings entry opens on this phone.
            found = entry_for(SERIAL, SETTINGS)
            check(found is not None, f"no menu entry of {SETTINGS} for {SERIAL}")
            assert found is not None
            path, entry = found
            note(f"{path.name}: Exec={entry.get('Exec')}")
            check(f" open --phone {SERIAL} " in entry.get("Exec", ""),
                  f"Exec={entry.get('Exec')!r} lacks --phone {SERIAL}")
            # 6. No shared entries are left.
            shared = [p.name for p in sorted(applications_dir().glob("lantern.*.desktop"))
                      if "\nX-Lantern-Serials=" in "\n" + p.read_text(errors="replace")]
            check(not shared, f"shared entries left: {shared}")
            # 7. The foreign file is untouched.
            check(foreign.exists() and foreign.read_bytes() == planted,
                  f"{foreign.name} was changed or removed")
            note(f"{foreign.name} unchanged")
            ok = True
        finally:
            # 8.
            cleanup(ok, ("remove the planted file", lambda: foreign.unlink(missing_ok=True)))


    def autoconnect_on_remembers_off(self):
        """With `autoconnect = true` (and LANTERN_SERIAL lifted, a 0 ms unplug grace): a phone
        switched off stays off across a replug and a lanternd restart; `lantern connect` turns
        it back on, and a replug then reconnects it by itself."""
        path = settings_file()
        saved_file = path.read_bytes() if path.exists() else None
        note(f"{path}: " + (f"saved ({len(saved_file)} bytes)" if saved_file is not None
                            else "absent"))

        def restore_file():
            if saved_file is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(saved_file)

        def stays_off(secs):
            deadline = time.monotonic() + secs
            while time.monotonic() < deadline:
                now = status()
                check(phone_status(SERIAL).get("state") == "off",
                      f"phone not off: {json.dumps(now)}")
                check(not (now["state"] == "ready" and now["serial"] == SERIAL),
                      f"the phone connected while off: {json.dumps(now)}")
                time.sleep(1)

        env = manager_env(set={"LANTERN_UNPLUG_GRACE_MS": "0"}, unset=["LANTERN_SERIAL"])
        ok = False
        entered = False
        try:
            # 1. Autoconnect on; the live flag set earlier in the suite reconnects the phone.
            text = (saved_file or b"").decode(errors="replace")
            lines = [l for l in text.splitlines() if not re.match(r"\s*autoconnect\s*=", l)]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("\n".join(lines + ["autoconnect = true"]) + "\n")
            # __enter__ puts the environment back itself if it fails part-way.
            env.__enter__()
            entered = True
            _, secs = wait_ready(15)
            note(f"autoconnect on: ready {secs:.1f} s after the restart")
            # 2. Switched off: the phone is restored within 5 s.
            note(f"lantern disconnect {SERIAL}: {lantern_cmd('disconnect', SERIAL)}")
            wait_until("phone restored", 5, lambda: restored(phone_state()), interval=0.2)
            # 3. A replug leaves it off.
            replug()
            stays_off(15)
            note("off for 15 s after the replug")
            # 4. So does a lanternd restart.
            systemctl("restart", UNIT)
            stays_off(15)
            note("off for 15 s after a lanternd restart")
            # 5. Connect: ready.
            note(f"lantern connect {SERIAL}: {lantern_cmd('connect', SERIAL)}")
            _, secs = wait_ready(15)
            note(f"ready {secs:.1f} s after connect")
            # 6. A replug reconnects it by itself.
            replug()
            _, secs = wait_ready(20)
            note(f"ready again {secs:.1f} s after the replug")
            ok = True
        finally:
            # 7. The file, then the environment (restarts lanternd); lanternd left ready. A
            # failure in 3-5 may have left the phone off: connect it again (wait_ready alone
            # passes through LANTERN_SERIAL, which ignores the off flag).
            steps = [("restore settings.toml", restore_file)]
            if entered:
                steps.append(("restore lanternd's environment",
                              lambda: env.__exit__(None if ok else Fail, None, None)))
            if not ok:
                steps.append(("lantern connect", lambda: lantern_cmd("connect", SERIAL)))
            steps.append(("lanternd ready", lambda: wait_ready(20)))
            cleanup(ok, *steps)


    def autoconnect_off_asks_on_plug(self):
        """With `autoconnect = false` (and LANTERN_SERIAL lifted, a 0 ms unplug grace): the
        restart reconnects the phone through its `live` flag without a "plugged in" notice; a
        replug then asks: a Notify "<phone> is plugged in" within 5 s, the phone waiting and
        restored; clicking its tray row connects it."""
        path = settings_file()
        saved_file = path.read_bytes() if path.exists() else None
        note(f"{path}: " + (f"saved ({len(saved_file)} bytes)" if saved_file is not None
                            else "absent"))

        def restore_file():
            if saved_file is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(saved_file)

        device = device_name()
        summary = f"{device} is plugged in"
        env = manager_env(set={"LANTERN_UNPLUG_GRACE_MS": "0"}, unset=["LANTERN_SERIAL"])
        ok = False
        entered = False
        with NotifyMonitor("25") as mon:
            try:
                # 1. Autoconnect off; the restart reconnects the phone through its live flag.
                edit_settings(saved_file, "autoconnect", "false")
                env.__enter__()  # puts the environment back itself if it fails part-way
                entered = True
                _, secs = wait_ready(15)
                note(f"autoconnect off: ready {secs:.1f} s after the restart (live flag)")
                time.sleep(5)
                check(not mon.matching(summary), f"{summary!r} after a restart")
                note(f"no {summary!r} for 5 s after the restart")
                # 2. A replug asks instead of connecting.
                before = len(mon.notifies())
                replug()
                got, secs = wait_until(f"Notify {summary!r}", 5,
                                       lambda: mon.matching(summary, before), interval=0.2)
                note(f"Notify {got[0][0]!r} {secs:.1f} s after the phone came back")
                wait_until("phone restored", 5, lambda: restored(phone_state()), interval=0.2)
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    p = phone_status(SERIAL)
                    check(p.get("state") == "waiting", f"phone not waiting: {json.dumps(p)}")
                    check(restored(phone_state()), "the phone is not restored while waiting")
                    time.sleep(1)
                note("waiting and restored for 10 s after the replug")
                # 3. Its tray row connects it (the bus name is taken after the restart).
                name = tray_name()
                (row, menu), _ = wait_until(f"a checkmark row {device!r}", 15,
                                            lambda: tray_phone_row(name, device))
                note(f"the phone's row is {row}: {menu[row]}")
                tray_click(name, row)
                _, secs = wait_ready(15)
                note(f"ready {secs:.1f} s after the tray click")
                ok = True
            finally:
                # 4. The file, then the environment (restarts lanternd); lanternd left ready.
                steps = [("restore settings.toml", restore_file)]
                if entered:
                    steps.append(("restore lanternd's environment",
                                  lambda: env.__exit__(None if ok else Fail, None, None)))
                if not ok:
                    steps.append(("lantern connect", lambda: lantern_cmd("connect", SERIAL)))
                steps.append(("lanternd ready", lambda: wait_ready(20)))
                cleanup(ok, *steps)

    def connect_from_entry_opens_app(self):
        """Manual (Sahaj clicks Connect; real desktop, not --nested: lanternd opens the window
        on the session it runs in). LANTERN_SERIAL lifted, a 0 ms unplug grace. The phone is
        disconnected in Lantern; `lantern open --phone SERIAL com.android.settings` exits 1
        within 5 s and a Notify "Connect your <phone>" with the `connect` action follows within
        5 s; after the click the phone is ready within 20 s, Settings opens in a window of its
        own app_id within 20 s, and 10 s later lanternd lists exactly one Settings window. The
        window is closed through KWin."""
        check(not NESTED, "item 26 needs the real desktop: lanternd opens the window there")
        path = settings_file()
        saved_file = path.read_bytes() if path.exists() else None
        note(f"{path}: " + (f"saved ({len(saved_file)} bytes)" if saved_file is not None
                            else "absent"))

        def restore_file():
            if saved_file is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(saved_file)

        def settings_windows():
            return [w for w in status()["windows"] if w["pkg"] == SETTINGS]

        def close_settings():
            if settings_windows():
                kwin_on(SETTINGS, CLOSE, "lantern_close")
                wait_until("Settings window gone", 10, lambda: not settings_windows())

        summary = f"Connect your {device_name()}"
        env = manager_env(set={"LANTERN_UNPLUG_GRACE_MS": "0"}, unset=["LANTERN_SERIAL"])
        ok = False
        entered = False
        with NotifyMonitor("26") as mon:
            try:
                env.__enter__()  # puts the environment back itself if it fails part-way
                entered = True
                _, secs = wait_ready(15)
                note(f"ready {secs:.1f} s after the restart (live flag)")
                # 1. Disconnected in Lantern: the phone is restored.
                note(f"lantern disconnect {SERIAL}: {lantern_cmd('disconnect', SERIAL)}")
                wait_until("phone restored", 5, lambda: restored(phone_state()), interval=0.2)
                # 2. The open is refused and offered.
                before = len(mon.notifies())
                t0 = time.monotonic()
                r = subprocess.run([LANTERN, "open", "--phone", SERIAL, SETTINGS],
                                   capture_output=True, text=True, timeout=5)
                took = time.monotonic() - t0
                note(f"lantern open --phone {SERIAL} {SETTINGS}: exit {r.returncode} "
                     f"after {took:.1f} s")
                check(r.returncode == 1, f"lantern open exited {r.returncode}, want 1")
                got, secs = wait_until(f"Notify {summary!r}", 5,
                                       lambda: mon.matching(summary, before), interval=0.2)
                note(f"Notify {got[0][0]!r} {secs:.1f} s after the open exited; "
                     f"actions {got[0][1]}")
                check("connect" in got[0][1][::2], f"no `connect` action: {got[0][1]}")
                # 3. Sahaj clicks Connect.
                print(f"    >>> Click Connect on the notification {summary!r}.", flush=True)
                # 4. Connected, and Settings opens once. The click clears `off` at once.
                _, secs = wait_until("the Connect click", 180,
                                     lambda: phone_status(SERIAL).get("state") != "off",
                                     interval=0.2)
                note(f"clicked {secs:.1f} s after the prompt")
                _, secs = wait_ready(20)
                note(f"ready {secs:.1f} s after the click")
                want = f"resourceClass={app_id_of(SETTINGS)} "
                lines, secs = wait_until(
                    "the Settings window", 20,
                    lambda: [l for l in kwin_on(SETTINGS, DESCRIBE, "lantern_describe")
                             if l.startswith(want)])
                note(f"{lines[0]} ({secs:.1f} s after ready)")
                time.sleep(10)
                wins = settings_windows()
                check(len(wins) == 1, f"{len(wins)} Settings windows after 10 s: {wins}")
                # 5. Closed through KWin.
                close_settings()
                note("Settings closed through KWin")
                ok = True
            finally:
                steps = [("close Settings", close_settings),
                         ("restore settings.toml", restore_file)]
                if entered:
                    steps.append(("restore lanternd's environment",
                                  lambda: env.__exit__(None if ok else Fail, None, None)))
                if not ok:
                    steps.append(("lantern connect", lambda: lantern_cmd("connect", SERIAL)))
                steps.append(("lanternd ready", lambda: wait_ready(20)))
                cleanup(ok, *steps)


ITEMS = [
    "session_starts_on_plug",
    "open_settings_creates_one_display",
    "second_open_focuses",
    "resize_reflows",
    "close_quits",
    "app_exit_closes_window",
    "daemon_kill_restores_phone",
    "usb_yank",
    "companion_linked",
    "catalogue_tracks_phone",
    "title_is_label",
    "ime_unicode_text",
    "clipboard_both_ways",
    "usb_yank_restores_keyboard",
    "phones_keep_their_entries",
    "not_on_this_phone_notice",
    "clipboard_setting_off",
    "keyboard_setting_off",
    "disconnect_and_connect",
    "tray_menu",
    "unlock_prompt",
    "swipe_phone_never_prompts",
    "per_phone_folders",
    "autoconnect_on_remembers_off",
    "autoconnect_off_asks_on_plug",
    "connect_from_entry_opens_app",
]
# Need a person: pull the cable (8, 14), type the phone's PIN (21) or click Connect (26).
MANUAL = {"usb_yank", "usb_yank_restores_keyboard", "unlock_prompt",
          "connect_from_entry_opens_app"}


def desktop_locked():
    """True if the graphical login session is locked (it then sends windows no frames)."""
    try:
        for sid in sh("loginctl", "list-sessions", "--no-legend").split("\n"):
            if sid.split():
                props = sh("loginctl", "show-session", sid.split()[0], "-p", "Type",
                           "-p", "LockedHint")
                if "Type=wayland" in props and "LockedHint=yes" in props:
                    return True
    except Fail:
        pass
    return False


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manual", action="store_true",
                    help="also run items 8, 14, 21 and 26 (USB unplug, typing the PIN, "
                         "clicking Connect; prompts)")
    ap.add_argument("--state", action="store_true", help="print phone state + status and exit")
    ap.add_argument("--only", nargs="*", help="run only these items (numbers or names)")
    ap.add_argument("--nested", action="store_true",
                    default=os.environ.get("LANTERN_TEST_NESTED") == "1",
                    help="run test windows in a private headless KWin (for a locked desktop)")
    ap.add_argument("--signal", choices=["TERM", "KILL"], default="TERM",
                    help="item 7: TERM (clean stop, default) or KILL (no cleanup; systemd "
                         "restarts lanternd and its new session must drop the stale forward)")
    args = ap.parse_args()
    global KILL_SIGNAL, NESTED, MANUAL_RUN
    KILL_SIGNAL = args.signal
    MANUAL_RUN = args.manual
    if args.state:
        try:
            st = status()
        except Fail as e:
            st = str(e)
        print(json.dumps({"status": st, "phone": phone_state()}, indent=1))
        return 0
    suite = Suite()
    items = [i for i in ITEMS if args.manual or i not in MANUAL]
    if args.only:
        unknown = [o for o in args.only if o not in ITEMS
                   and not (o.isdigit() and 1 <= int(o) <= len(ITEMS))]
        if unknown:
            ap.error(f"unknown items {unknown}; items: " +
                     ", ".join(f"{n} {i}" for n, i in enumerate(ITEMS, 1)))
        items = [i for n, i in enumerate(ITEMS, 1) if i in args.only or str(n) in args.only]
    print(f"lantern device test: serial={SERIAL} binary={LANTERN} logs={LOG_DIR}", flush=True)
    if not args.nested and desktop_locked():
        print("WARNING: the Plasma session is locked; windows get no frames, so the window items "
              "(4-6, 12, 13, 17, 22) cannot pass. Unlock it or use --nested.", flush=True)
    results = []
    # Preflight: lanternd serves only this phone for the run (other phones plugged in are
    # ignored); the previous value is put back at the end.
    previous = systemd_env().get("LANTERN_SERIAL")
    try:
        # Inside the try: a failed set or restart is still put back by the `finally`.
        if previous != SERIAL:
            systemctl("set-environment", f"LANTERN_SERIAL={SERIAL}")
            systemctl("restart", UNIT)
        print(f"lanternd LANTERN_SERIAL={SERIAL} for this run", flush=True)
        if args.nested:
            NESTED = Nested()
            print(f"nested KWin: WAYLAND_DISPLAY={NESTED.socket_path} log={NESTED.log} "
                  f"config={NESTED.home}", flush=True)
        for name in items:
            idx = ITEMS.index(name) + 1
            print(f"[{idx}] {name}", flush=True)
            t0 = time.monotonic()
            try:
                getattr(suite, name)()
                ok, why = True, ""
            except Exception as e:  # any failure is this item's FAIL; the rest still run
                ok, why = False, f": {type(e).__name__}: {e}"
            results.append(ok)
            print(f"{'PASS' if ok else 'FAIL'} {idx} {name} ({time.monotonic() - t0:.1f} s){why}",
                  flush=True)
            if not ok and suite.settings and suite.settings.p.poll() is None:
                # Close the Settings window a failed item left open (KWin close = quit the app),
                # then make sure its process is gone.
                try:
                    kwin_on(SETTINGS, CLOSE, "lantern_close")
                except Exception as e:
                    note(f"could not close Settings through KWin: {e}")
                suite.settings.kill()
    finally:
        # Whatever happened (including Ctrl-C), no `lantern open` process outlives the suite, the
        # nested session is stopped and the test screen is disabled; each step is guarded so a
        # failing one never skips the next.
        for w in Window.every:
            try:
                w.kill()
            except Exception as e:
                print(f"WARNING: could not stop a {w.pkg} window: {e}", flush=True)
        if NESTED:
            try:
                NESTED.stop()
            except Exception as e:
                print(f"WARNING: nested KWin stop failed: {e}", flush=True)
                shutil.rmtree(NESTED.home, ignore_errors=True)
        if {"catalogue_tracks_phone", "ime_unicode_text", "clipboard_both_ways",
            "clipboard_setting_off", "unlock_prompt", "swipe_phone_never_prompts"} & set(items):
            try:  # the test screen is never left enabled
                test_activity(False)
            except Exception as e:
                print(f"WARNING: could not disable the companion's test screen: {e}", flush=True)
        if previous != SERIAL:
            # lanternd's environment as it was before the run, each step guarded.
            try:
                if previous is None:
                    systemctl("unset-environment", "LANTERN_SERIAL")
                else:
                    systemctl("set-environment", f"LANTERN_SERIAL={previous}")
            except Exception as e:
                print(f"WARNING: could not put back LANTERN_SERIAL: {e}", flush=True)
            try:
                systemctl("restart", UNIT)
            except Exception as e:
                print(f"WARNING: could not restart {UNIT}: {e}", flush=True)
    if not args.manual and not args.only:
        for name in sorted(MANUAL, key=ITEMS.index):
            print(f"SKIP {ITEMS.index(name) + 1} {name} (needs a person: run with --manual)")
    print(f"{sum(results)}/{len(results)} PASS", flush=True)
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
