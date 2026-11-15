#!/usr/bin/env python3
"""Self-tests of tools/device_test.py's pure parts. No phone, adb, lanternd or desktop tool runs:
the module is imported with TMPDIR in a scratch dir (its LOG_DIR lands there) and its `sh` and
its subprocess run/Popen are replaced by ones that fail the test; XDG config/state/data dirs
point into the scratch dir while the tests run.

    TMPDIR=$(mktemp -d) python3 -m unittest tools/test_device_test.py -v
"""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import json
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
SCRATCH = tempfile.mkdtemp(prefix="lantern-device-selftest-")


def _load(env_over=None):
    """A fresh device_test module, imported with TMPDIR in SCRATCH (and `env_over` applied:
    a None value removes the variable) and the environment put back afterwards."""
    over = dict(env_over or {}, TMPDIR=SCRATCH)
    saved = {k: os.environ.get(k) for k in over}
    for k, v in over.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    tempfile.tempdir = None  # re-read TMPDIR
    try:
        spec = importlib.util.spec_from_file_location("device_test", HERE / "device_test.py")
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
    finally:
        for k, v in saved.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        tempfile.tempdir = None
    _seal(mod)
    return mod


def _no_subprocess(*args, **kwargs):
    raise AssertionError(f"a self-test reached a subprocess: {args}")


def _seal(mod):
    """Every door to a subprocess in `mod` fails the test: its `sh`, and a private copy of the
    subprocess module whose run/Popen/call/check_* raise (the real module is left alone)."""
    proxy = types.ModuleType("subprocess")
    proxy.__dict__.update(subprocess.__dict__)
    for name in ("run", "Popen", "call", "check_call", "check_output", "getoutput",
                 "getstatusoutput"):
        setattr(proxy, name, _no_subprocess)
    mod.subprocess = proxy
    mod.sh = _no_subprocess


dt = _load()
XDG = {var: os.path.join(SCRATCH, var.lower()) for var in
       ("XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME")}
_saved_xdg = {}


def setUpModule():
    # Any path the module derives (settings.toml, device records, menu entries) lands in SCRATCH.
    for var, d in XDG.items():
        _saved_xdg[var] = os.environ.get(var)
        os.makedirs(d, exist_ok=True)
        os.environ[var] = d


def tearDownModule():
    for var, v in _saved_xdg.items():
        if v is None:
            os.environ.pop(var, None)
        else:
            os.environ[var] = v
    shutil.rmtree(SCRATCH, ignore_errors=True)


class patched:
    """Sets module attributes for a `with` block and puts them back."""

    def __init__(self, mod, **attrs):
        self.mod, self.attrs, self.saved = mod, attrs, {}

    def __enter__(self):
        for k, v in self.attrs.items():
            self.saved[k] = getattr(self.mod, k)
            setattr(self.mod, k, v)
        return self

    def __exit__(self, *exc):
        for k, v in self.saved.items():
            setattr(self.mod, k, v)
        return False


def released_state(**over):
    st = {"panel": "Off", "wakefulness": "Asleep", "displays": ["0"], "forwards": [],
          "helper_pids": []}
    st.update(over)
    return st


class DeviceTestSelfTests(unittest.TestCase):
    def test_log_dir_is_in_scratch(self):
        self.assertTrue(str(dt.LOG_DIR).startswith(SCRATCH), dt.LOG_DIR)

    def test_sealed(self):
        with self.assertRaises(AssertionError):
            dt.subprocess.run(["true"])
        with self.assertRaises(AssertionError):
            dt.subprocess.Popen(["true"])
        self.assertTrue(str(dt.settings_file()).startswith(SCRATCH), dt.settings_file())
        self.assertTrue(str(dt.devices_dir()).startswith(SCRATCH), dt.devices_dir())

    def test_released_accepts_a_sleeping_phone(self):
        self.assertTrue(dt.released(released_state()))
        self.assertTrue(dt.released(released_state(wakefulness="Dozing")))
        self.assertTrue(dt.released(released_state(panel="On", wakefulness="Awake")))
        self.assertFalse(dt.released(released_state(wakefulness="Awake")))
        self.assertFalse(dt.released(released_state(displays=["0", "12"])))
        self.assertFalse(dt.released(released_state(forwards=["x tcp:1 localabstract:lantern"])))
        self.assertFalse(dt.released(released_state(helper_pids=["123"])))

    def test_unknown_wakefulness_is_not_released(self):
        self.assertTrue(dt.released(released_state(wakefulness="Dreaming")))
        for raw in ("?", "Unknown"):
            st = released_state(wakefulness=raw)
            self.assertFalse(dt.released(st), raw)
            self.assertTrue(any(repr(raw) in r for r in dt.held_by(st)), dt.held_by(st))
            # The panel on: the wakefulness doesn't matter.
            self.assertTrue(dt.released(dict(st, panel="On")), raw)
        self.assertEqual(dt.held_by(released_state()), [])
        self.assertIn("Asleep", dt.asleep_note(released_state()))
        self.assertEqual(dt.asleep_note(released_state(panel="On", wakefulness="Awake")), "")

    def test_wait_phone_names_why_it_is_held(self):
        st = released_state(wakefulness="Weird")
        with patched(dt, phone_state=lambda serial=None: st):
            with self.assertRaises(dt.Fail) as held:
                dt.wait_phone("phoneA released", 0.3, dt.released, "phoneA")
        self.assertIn("'Weird'", str(held.exception))

    def test_restored_uses_the_same_panel_rule(self):
        base = {"stay_on": "0", "ime": "com.x/.Ime", "imes": "com.x/.Ime"}
        saved = dict(dt.BASELINES)
        dt.BASELINES.clear()
        dt.BASELINES["phoneA"] = dict(base)
        try:
            st = released_state(stay_on="0", ime="com.x/.Ime", imes="com.x/.Ime", journal=False)
            self.assertTrue(dt.restored(st, "phoneA"))
            self.assertTrue(dt.restored(dict(st, wakefulness="Dozing"), "phoneA"))
            self.assertTrue(dt.restored(dict(st, panel="On", wakefulness="Awake"), "phoneA"))
            self.assertFalse(dt.restored(dict(st, wakefulness="Awake"), "phoneA"))
            self.assertFalse(dt.restored(dict(st, stay_on="7"), "phoneA"))
            self.assertFalse(dt.restored(dict(st, ime=dt.LANTERN_IME), "phoneA"))
            self.assertFalse(dt.restored(dict(st, journal=True), "phoneA"))
            self.assertFalse(dt.restored(dict(st, displays=["0", "3"]), "phoneA"))
        finally:
            dt.BASELINES.clear()
            dt.BASELINES.update(saved)

    def test_parse_wakefulness(self):
        self.assertEqual(dt.parse_wakefulness("  mWakefulness=Asleep\n"), "Asleep")
        dump = "x\n  mWakefulness=Dozing\n  mWakefulnessChanging=false\n"
        self.assertEqual(dt.parse_wakefulness(dump), "Dozing")
        self.assertEqual(dt.parse_wakefulness("POWER MANAGER\n  mWakefulnessChanging=false\n"), "?")
        self.assertEqual(dt.parse_wakefulness(""), "?")

    def test_pick_other(self):
        text = "List of devices attached\n20e9318\tdevice\nZD222LMDFV\tdevice\n"
        self.assertEqual(dt.pick_other(text, "20e9318"), "ZD222LMDFV")
        self.assertEqual(dt.pick_other(text + "AAA\tunauthorized\nBBB\toffline\n", "20e9318"),
                         "ZD222LMDFV")
        with self.assertRaises(dt.Fail) as none:
            dt.pick_other("List of devices attached\n20e9318\tdevice\nAAA\tunauthorized\n",
                          "20e9318")
        self.assertIn("none", str(none.exception))
        with self.assertRaises(dt.Fail) as two:
            dt.pick_other(text + "CCC\tdevice\n", "20e9318")
        self.assertIn("ZD222LMDFV", str(two.exception))
        self.assertIn("CCC", str(two.exception))

    def test_manual_plan_lists_every_step_up_front(self):
        out = dt.manual_plan(["usb_yank", "unlock_prompt"])
        text = "\n".join(out)
        pos = 0
        for name in ("usb_yank", "unlock_prompt"):
            idx = dt.ITEMS.index(name) + 1
            header = [l for l in out if f"[{idx}] {name}" in l]
            self.assertEqual(len(header), 1, out)
            at = text.index(header[0], pos)
            self.assertGreaterEqual(at, pos)
            pos = at
            for step in dt.STEPS[name]:
                at = text.index(step, pos)
                self.assertGreaterEqual(at, pos, step)
                pos = at
        self.assertEqual(dt.manual_plan(["usb_yank", "companion_linked", "unlock_prompt"]), out)
        self.assertEqual(dt.manual_plan(["companion_linked"]), [])

    def test_every_manual_item_has_steps(self):
        self.assertEqual(set(dt.STEPS), dt.MANUAL)
        for name, steps in dt.STEPS.items():
            self.assertTrue(steps and all(isinstance(s, str) and s for s in steps), name)

    def test_no_stdin_waits(self):
        text = (HERE / "device_test.py").read_text()
        self.assertFalse("input(" in text, "device_test.py waits on stdin (input()")

    def test_manual_wait_default(self):
        self.assertEqual(_load({"LANTERN_MANUAL_WAIT": None}).MANUAL_WAIT, 3600)
        self.assertEqual(_load({"LANTERN_MANUAL_WAIT": "42"}).MANUAL_WAIT, 42)

    def test_pair_items_need_pair(self):
        self.assertTrue(set(dt.PAIR) <= set(dt.ITEMS))

    def test_items_28_to_32_are_pair_items(self):
        names = dt.ITEMS[27:32]
        self.assertEqual(names, ["two_phones_live", "clipboard_fans_out",
                                 "one_phone_failure_isolated", "unplug_one_keeps_other",
                                 "lock_one_keeps_other"])
        self.assertEqual(set(names), dt.PAIR)
        self.assertEqual(dt.MANUAL & dt.PAIR, {"unplug_one_keeps_other", "lock_one_keeps_other"})
        self.assertEqual(dt.STEPS["lock_one_keeps_other"],
                         ["When 'Unlock your edge 50 pro' shows, click Enter PIN… and type "
                          "the PIN."])
        for name in dt.ITEMS:
            self.assertTrue(callable(getattr(dt.Suite, name, None)), name)

    def test_items_33_and_34(self):
        self.assertEqual(dt.ITEMS[32:34], ["replug_keeps_requested_app", "notices_close_on_quit"])
        self.assertIn("replug_keeps_requested_app", dt.MANUAL)
        self.assertNotIn("notices_close_on_quit", dt.MANUAL)
        self.assertFalse({"replug_keeps_requested_app", "notices_close_on_quit"} & dt.PAIR)
        self.assertEqual(dt.STEPS["replug_keeps_requested_app"],
                         ["Pull the Xiaomi's cable.",
                          "When 'Connect your Xiaomi 11T Pro' shows, plug the phone in.",
                          "On 'Xiaomi 11T Pro is plugged in', click Connect."])

    def test_monitor_calls_pairs_notify_with_its_reply(self):
        log = """\u2023 Type=method_call  Endian=l  Flags=0  Version=1 Cookie=12  Timestamp="x"
  Sender=:1.55  Destination=org.freedesktop.Notifications  Path=/org/freedesktop/Notifications  Interface=org.freedesktop.Notifications  Member=Notify
  UniqueName=:1.55
  MESSAGE "susssasa{sv}i" {
          STRING "Lantern";
          UINT32 0;
          STRING "smartphone";
          STRING "Connect your Xiaomi 11T Pro";
          STRING "Settings is on your Xiaomi 11T Pro. Connect it to open Settings here.";
          ARRAY "s" {
                  STRING "connect";
                  STRING "Connect";
          };
          ARRAY "{sv}" {
          };
          INT32 -1;
  };

\u2023 Type=method_call  Endian=l  Flags=0  Version=1 Cookie=7  Timestamp="x"
  Sender=:1.80  Destination=org.freedesktop.Notifications  Path=/org/freedesktop/Notifications  Interface=org.freedesktop.Notifications  Member=Notify
  MESSAGE "susssasa{sv}i" {
          STRING "other";
          UINT32 0;
          STRING "";
          STRING "Someone else";
          STRING "";
          ARRAY "s" {
          };
  };

\u2023 Type=method_return  Endian=l  Flags=1  Version=1 Cookie=40  ReplyCookie=7  Timestamp="x"
  Sender=:1.20  Destination=:1.80
  MESSAGE "u" {
          UINT32 99;
  };

\u2023 Type=method_return  Endian=l  Flags=1  Version=1 Cookie=41  ReplyCookie=12  Timestamp="x"
  Sender=:1.20  Destination=:1.55
  MESSAGE "u" {
          UINT32 17;
  };

\u2023 Type=method_call  Endian=l  Flags=0  Version=1 Cookie=13  Timestamp="x"
  Sender=:1.55  Destination=org.freedesktop.Notifications  Path=/org/freedesktop/Notifications  Interface=org.freedesktop.Notifications  Member=CloseNotification
  MESSAGE "u" {
          UINT32 17;
  };
"""
        calls = dt.monitor_calls(log)
        self.assertEqual([c["member"] for c in calls], ["Notify", "Notify", "CloseNotification"])
        self.assertEqual(calls[0]["summary"], "Connect your Xiaomi 11T Pro")
        self.assertEqual(calls[0]["body"],
                         "Settings is on your Xiaomi 11T Pro. Connect it to open Settings here.")
        self.assertEqual(calls[0]["actions"], ["connect", "Connect"])
        self.assertEqual(calls[0]["id"], 17)
        self.assertEqual(calls[1]["id"], 99)
        self.assertEqual(calls[2]["id"], 17)
        # A Notify whose reply isn't in the log yet has no id.
        cut = log[:log.index("Cookie=41")]
        self.assertIsNone(dt.monitor_calls(cut)[0]["id"])

    def test_monitor_calls_reads_header_keys_from_header_lines_only(self):
        """R10 (3): a body string that looks like header keys never redirects a call."""
        log = """\u2023 Type=method_call  Endian=l  Flags=0  Version=1 Cookie=12  Timestamp="x"
  Sender=:1.55  Destination=org.freedesktop.Notifications  Path=/org/freedesktop/Notifications  Interface=org.freedesktop.Notifications  Member=Notify
  UniqueName=:1.55
  MESSAGE "susssasa{sv}i" {
          STRING "Lantern";
          UINT32 0;
          STRING "smartphone";
          STRING "A Cookie=99 Sender=:1.9 is plugged in";
          STRING "Type=method_return Member=CloseNotification ReplyCookie=3";
          ARRAY "s" {
          };
          ARRAY "{sv}" {
          };
          INT32 -1;
  };

\u2023 Type=method_return  Endian=l  Flags=1  Version=1 Cookie=41  ReplyCookie=12  Timestamp="x"
  Sender=:1.20  Destination=:1.55
  MESSAGE "u" {
          UINT32 17;
  };
"""
        calls = dt.monitor_calls(log)
        self.assertEqual([c["member"] for c in calls], ["Notify"])
        self.assertEqual(calls[0]["summary"], "A Cookie=99 Sender=:1.9 is plugged in")
        self.assertEqual(calls[0]["id"], 17)

    def test_unplugged_pattern_names_one_phone(self):
        import re
        rx = re.compile(dt.unplugged_pattern("20e9318"))
        line = ' INFO lantern::daemon: unplugged (offline past the grace) serial="20e9318"'
        self.assertTrue(rx.search(line))
        self.assertTrue(rx.search(line.replace('"20e9318"', "20e9318")))
        self.assertFalse(rx.search(line.replace("20e9318", "ZD222LMDFV")))
        self.assertFalse(rx.search(line.replace("20e9318", "20e93189")))
        self.assertFalse(rx.search(line.replace("unplugged", "plugged")))

    def test_unlock_ok_pattern_names_one_phone(self):
        import re
        rx = re.compile(dt.unlock_ok_pattern("ZD222LMDFV"))
        line = ' INFO lantern::daemon::ipc: unlock result serial="ZD222LMDFV" ok=true reason=""'
        self.assertTrue(rx.search(line))
        self.assertTrue(rx.search(line.replace('"ZD222LMDFV"', "ZD222LMDFV")))
        self.assertFalse(rx.search(line.replace("ZD222LMDFV", "20e9318")))
        self.assertFalse(rx.search(line.replace("ZD222LMDFV", "ZD222LMDFV2")))
        self.assertFalse(rx.search(line.replace("ok=true", "ok=false")))

    def test_pair_cleanup_runs_every_step_in_order(self):
        ran = []

        def rec(name, fail=False):
            def fn():
                ran.append(name)
                if fail:
                    raise dt.Fail(f"{name} broke")
            return fn
        names = ["clipboard", "settings", "stop", "released", "records", "env", "ready"]
        steps = dt.pair_cleanup_steps(
            restore_clipboard=rec("clipboard"), restore_settings=rec("settings", fail=True),
            stop_daemon=rec("stop"), wait_released=rec("released"),
            restore_records=rec("records"), restore_env=rec("env"), wait_back=rec("ready"))
        self.assertEqual(len(steps), 7)
        self.assertTrue(all(isinstance(what, str) and what for what, _ in steps))
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            dt.cleanup(False, *steps)  # the item failed: step errors are only noted
        self.assertEqual(ran, names)
        self.assertIn("cleanup failed: restore settings.toml: Fail: settings broke", out.getvalue())
        ran.clear()
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(dt.Fail):
            dt.cleanup(True, *steps)  # the item passed: the cleanup failure is raised
        self.assertEqual(ran, names)

    def test_two_phones_refuses_before_changing_anything(self):
        calls = []

        def fake_sh(*args, **kwargs):
            calls.append(args)
            if args[:2] == ("adb", "devices"):
                return f"List of devices attached\n{dt.SERIAL}\tdevice\nZZZ\tunauthorized\n"
            raise AssertionError(f"two_phones changed something before refusing: {args}")
        saved_other, saved_sh = dt.OTHER, dt.sh
        dt.OTHER, dt.sh = "ZD222LMDFV", fake_sh
        try:
            with self.assertRaises(dt.Fail) as refused:
                with dt.two_phones():
                    self.fail("entered without both phones")
        finally:
            dt.OTHER, dt.sh = saved_other, saved_sh
        self.assertIn("ZD222LMDFV", str(refused.exception))
        self.assertEqual(calls, [("adb", "devices")])


    def two_phones_exit(self, entered):
        """two_phones.__exit__ after a failed __enter__ (entered False) or a used one: the
        systemctl calls it made, in order."""
        calls = []

        class Env:
            def __exit__(self, *exc):
                calls.append(("env exit",))
                calls.append(("restart", dt.UNIT))

        def fake_systemctl(*args, check_rc=True):
            calls.append(args)
            return ""
        with patched(dt, OTHER="ZD222LMDFV", systemctl=fake_systemctl,
                     phone_state=lambda serial=None: released_state(),
                     wait_daemon=lambda timeout=10: {}, wait_ready=lambda timeout=10: ({}, 0)):
            pair = dt.two_phones()
            pair.entered = entered
            pair.env = Env() if entered else None
            with contextlib.redirect_stdout(io.StringIO()):
                pair.__exit__(dt.Fail, dt.Fail("step 4 broke"), None)
        return calls

    def test_two_phones_failed_enter_starts_lanternd_again(self):
        calls = self.two_phones_exit(entered=False)
        self.assertEqual(calls[0], ("stop", dt.UNIT))
        self.assertIn(calls[-1], [("start", dt.UNIT), ("restart", dt.UNIT)], calls)
        self.assertEqual(len(calls), 2, calls)

    def test_two_phones_exit_restores_the_env(self):
        calls = self.two_phones_exit(entered=True)
        self.assertEqual(calls, [("stop", dt.UNIT), ("env exit",), ("restart", dt.UNIT)])

    def wait_idle(self, states, adb_states):
        states, adb_states = iter(states), iter(adb_states)
        with patched(dt, MANUAL_WAIT=10, status=lambda: {"state": next(states)},
                     adb_state=lambda serial=None: next(adb_states)):
            with contextlib.redirect_stdout(io.StringIO()):
                return dt.wait_idle_unplugged("phoneA")

    def test_wait_idle_unplugged(self):
        self.assertGreaterEqual(self.wait_idle(["ready", "idle"], [None, None]), 0)

    def test_replug_within_the_grace_fails_at_once(self):
        with self.assertRaises(dt.Fail) as early:
            self.wait_idle(["ready", "ready", "ready"], [None, "device", "device"])
        self.assertIn("plugged back within the unplug grace; redo with the cable out at least "
                      "15 s", str(early.exception))

    def test_write_record_flags(self):
        path = dt.record_path("phoneR")
        path.unlink(missing_ok=True)
        with contextlib.redirect_stdout(io.StringIO()):
            dt.write_record_flags("phoneR", {"off": dt.ABSENT, "live": dt.ABSENT})
        self.assertFalse(path.exists(), "an all-ABSENT restore created the record")
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"off": True, "name": "x"}))
        synced = []
        real_fsync = os.fsync

        def fsync(fd):
            synced.append(fd)
            return real_fsync(fd)
        with mock.patch.object(dt.os, "fsync", fsync), \
                contextlib.redirect_stdout(io.StringIO()):
            dt.write_record_flags("phoneR", {"off": dt.ABSENT, "live": True})
        self.assertEqual(json.loads(path.read_text()), {"name": "x", "live": True})
        self.assertTrue(synced, "the temp file was not fsynced")
        self.assertEqual([p.name for p in path.parent.iterdir() if "tmp" in p.name], [])

    def run_main(self, argv, sh=_no_subprocess):
        err = io.StringIO()
        with patched(dt, sh=sh, OTHER=None), mock.patch.object(sys, "argv", argv), \
                contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            try:
                rc = dt.main()
            except SystemExit as e:
                rc = e.code
        return rc, err.getvalue()

    def test_only_with_pair_names_dropped_items(self):
        solo = next(i for i in dt.ITEMS if i not in dt.PAIR)
        rc, err = self.run_main(["device_test.py", "--pair", "--only", solo])
        self.assertEqual(rc, 2)
        self.assertIn(solo, err)
        pair = dt.ITEMS[-1]  # PAIR fills in later tasks: stand one item in for a pair item
        with patched(dt, PAIR={pair}):
            rc, err = self.run_main(["device_test.py", "--only", pair])
        self.assertEqual(rc, 2)
        self.assertIn(pair, err)

    def test_pair_without_a_second_phone_exits_cleanly(self):
        def fake_sh(*args, **kwargs):
            if args[:2] == ("adb", "devices"):
                return f"List of devices attached\n{dt.SERIAL}\tdevice\n"
            raise AssertionError(f"main went on after pick_other failed: {args}")
        rc, err = self.run_main(["device_test.py", "--pair"], sh=fake_sh)
        self.assertEqual(rc, 2)
        self.assertIn("--pair needs a second phone", err)
        self.assertNotIn("Traceback", err)


if __name__ == "__main__":
    unittest.main()
