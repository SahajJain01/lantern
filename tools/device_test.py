#!/usr/bin/env python3
"""Lantern acceptance suite on a real phone + KDE Plasma (Wayland) desktop.

    python3 tools/build.py install && python3 tools/device_test.py            # items 1-7
    python3 tools/device_test.py --manual                                     # 1-7, then 8 (prompts)
    python3 tools/device_test.py --state                                      # print phone state

Needs: lanternd installed as the systemd user unit (tools/build.py install), the phone
(LANTERN_SERIAL, default 20e9318) authorized over USB, and KWin's scripting D-Bus (qdbus6).
Opens only com.android.settings and org.lineageos.jelly, each for a few seconds, and touches
no other window. Prints one PASS/FAIL line per item; exits 1 if any item failed.
"""
import argparse
import json
import os
from pathlib import Path
import re
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
# The phone's own stay_on_while_plugged_in value, which a session must restore. Item 1 reads it
# with the daemon stopped (or set LANTERN_STAY_ON_RESTORED to skip that).
STAY_ON_RESTORED = os.environ.get("LANTERN_STAY_ON_RESTORED") or None
STAY_ON_SESSION = "7"  # Session.STAY_ON in the helper
JOURNAL = "/data/local/tmp/lantern-session.json"  # JournalStore.PATH in the helper
RESIZE_LOGICAL = 900
KILL_SIGNAL = "TERM"  # item 7; --signal=KILL
LOG_DIR = Path(tempfile.mkdtemp(prefix="lantern-device-test-"))
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class Fail(Exception):
    pass


def check(cond, msg):
    if not cond:
        raise Fail(msg)


def note(msg):
    print(f"    {msg}", flush=True)


def sh(*args, timeout=20, check_rc=True):
    r = subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=timeout)
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


def systemctl(*args, check_rc=True):
    return sh("systemctl", "--user", *args, timeout=60, check_rc=check_rc)


def wait_ready(timeout=10):
    st, secs = wait_until("daemon ready", timeout, lambda: (s := status())["state"] == "ready" and s)
    return st, secs


class Window:
    """One `lantern open <pkg>` process, stderr logged to a file."""

    every = []  # all windows started, so the suite can kill whatever is left on any exit

    def __init__(self, pkg, tag, debug_input=None):
        Window.every.append(self)
        self.pkg = pkg
        self.log = LOG_DIR / f"{tag}.log"
        env = dict(os.environ, LANTERN_LOG="info", NO_COLOR="1")
        if debug_input:
            env["LANTERN_DEBUG_INPUT"] = debug_input
        self.f = open(self.log, "w")
        self.p = subprocess.Popen([LANTERN, "open", pkg], stdout=self.f, stderr=self.f, env=env)

    def lines(self):
        return [ANSI.sub("", l) for l in self.log.read_text().splitlines()]

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
        "panel": panel[0] if panel else "?",
        "displays": display_ids(),
        "forwards": forwards,
        "helper_pids": helpers,
        # The session journal exists exactly while a session is live (or after one nothing
        # could restore).
        "journal": adb("shell", f"ls {JOURNAL} 2>/dev/null || true").strip() == JOURNAL,
    }


def restored(st):
    return (st["stay_on"] == stay_on_baseline() and st["panel"] == "On"
            and st["displays"] == ["0"] and not st["forwards"] and not st["helper_pids"]
            and not st["journal"])


def released(st):
    """No session on the phone (the guard, which restores the setting, has exited too)."""
    return (st["panel"] == "On" and st["displays"] == ["0"] and not st["forwards"]
            and not st["helper_pids"])


def read_stay_on_baseline():
    """Stops lanternd, reads the phone's own stay-awake value, and leaves lanternd stopped."""
    global STAY_ON_RESTORED
    systemctl("stop", UNIT)
    st, _ = wait_until("phone released after stop", 10, lambda: released(s := phone_state()) and s,
                       interval=0.2)
    if STAY_ON_RESTORED is None:
        STAY_ON_RESTORED = st["stay_on"]
    note(f"baseline with lanternd stopped: stay_on={st['stay_on']} (expected restore value "
         f"{STAY_ON_RESTORED})")


def stay_on_baseline():
    if STAY_ON_RESTORED is None:
        try:
            read_stay_on_baseline()
        finally:
            systemctl("start", UNIT)
        wait_ready(15)
    return str(STAY_ON_RESTORED)


def armed(st):
    """The phone as a ready session with no windows leaves it."""
    return (st["stay_on"] == STAY_ON_SESSION and st["panel"] == "Off" and st["displays"] == ["0"]
            and len(st["forwards"]) == 1 and st["helper_pids"] and st["journal"])


# --- KWin scripting ------------------------------------------------------------------------

def kwin(js, name):
    """Runs a KWin script; returns the lines it print()ed (tagged), read back from the journal."""
    tag = f"LANTERN-T-{os.getpid()}-{time.monotonic_ns()}"
    path = LOG_DIR / f"{name}.js"
    path.write_text(js.replace("TAG", json.dumps(tag)))
    since = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    sh("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", name,
       check_rc=False)
    sid = sh("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.loadScript",
             path, name).strip()
    sh("qdbus6", "org.kde.KWin", f"/Scripting/Script{sid}", "org.kde.kwin.Script.run")
    try:
        out, _ = wait_until(f"kwin script {name} output", 3, lambda: [
            l.split(tag, 1)[1].strip()
            for l in sh("journalctl", "--user", "--since", since, "--no-pager", "-o", "cat")
            .splitlines() if tag in l])
    finally:
        sh("qdbus6", "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting.unloadScript", name,
           check_rc=False)
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
    js = js.replace("CLASS", json.dumps(f"lantern.{pkg}"))
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
        check(f"resourceClass=lantern.{SETTINGS}" in desc[1], "app_id mismatch")

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
        steps = [f["step"] if "step" in f else f for _, f in w.find("debug input")]
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


ITEMS = [
    "session_starts_on_plug",
    "open_settings_creates_one_display",
    "second_open_focuses",
    "resize_reflows",
    "close_quits",
    "app_exit_closes_window",
    "daemon_kill_restores_phone",
    "usb_yank",
]


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manual", action="store_true", help="also run item 8 (usb_yank, prompts)")
    ap.add_argument("--state", action="store_true", help="print phone state + status and exit")
    ap.add_argument("--only", nargs="*", help="run only these items")
    ap.add_argument("--signal", choices=["TERM", "KILL"], default="TERM",
                    help="item 7: TERM (clean stop, default) or KILL (no cleanup; systemd "
                         "restarts lanternd and its new session must drop the stale forward)")
    args = ap.parse_args()
    global KILL_SIGNAL
    KILL_SIGNAL = args.signal
    if args.state:
        try:
            st = status()
        except Fail as e:
            st = str(e)
        print(json.dumps({"status": st, "phone": phone_state()}, indent=1))
        return 0
    suite = Suite()
    items = ITEMS if args.manual else ITEMS[:-1]
    if args.only:
        items = [i for i in ITEMS if i in args.only]
    print(f"lantern device test: serial={SERIAL} binary={LANTERN} logs={LOG_DIR}", flush=True)
    results = []
    try:
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
        # Whatever happened (including Ctrl-C), no `lantern open` process outlives the suite.
        for w in Window.every:
            w.kill()
    if not args.manual and not args.only:
        print("SKIP 8 usb_yank (needs a person: run with --manual)")
    print(f"{sum(results)}/{len(results)} PASS", flush=True)
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
