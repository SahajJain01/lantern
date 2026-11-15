#!/usr/bin/env python3
"""Self-tests of tools/device_test.py's pure parts. No phone, adb, lanternd or desktop tool runs:
the module is imported with TMPDIR in a scratch dir (its LOG_DIR lands there) and its `sh`, the
only door to a subprocess these tests could reach, is replaced by one that fails the test.

    TMPDIR=$(mktemp -d) python3 -m unittest tools/test_device_test.py -v
"""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import shutil
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRATCH = tempfile.mkdtemp(prefix="lantern-device-selftest-")
_saved_tmpdir = os.environ.get("TMPDIR")
os.environ["TMPDIR"] = SCRATCH
tempfile.tempdir = None  # re-read TMPDIR
try:
    _spec = importlib.util.spec_from_file_location("device_test", HERE / "device_test.py")
    dt = importlib.util.module_from_spec(_spec)
    _spec.loader.exec_module(dt)
finally:
    if _saved_tmpdir is None:
        os.environ.pop("TMPDIR", None)
    else:
        os.environ["TMPDIR"] = _saved_tmpdir
    tempfile.tempdir = None


def _no_subprocess(*args, **kwargs):
    raise AssertionError(f"a self-test reached sh{args}")


dt.sh = _no_subprocess


def released_state(**over):
    st = {"panel": "Off", "wakefulness": "Asleep", "displays": ["0"], "forwards": [],
          "helper_pids": []}
    st.update(over)
    return st


class DeviceTestSelfTests(unittest.TestCase):
    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(SCRATCH, ignore_errors=True)

    def test_log_dir_is_in_scratch(self):
        self.assertTrue(str(dt.LOG_DIR).startswith(SCRATCH), dt.LOG_DIR)

    def test_released_accepts_a_sleeping_phone(self):
        self.assertTrue(dt.released(released_state()))
        self.assertTrue(dt.released(released_state(wakefulness="Dozing")))
        self.assertTrue(dt.released(released_state(panel="On", wakefulness="Awake")))
        self.assertFalse(dt.released(released_state(wakefulness="Awake")))
        self.assertFalse(dt.released(released_state(displays=["0", "12"])))
        self.assertFalse(dt.released(released_state(forwards=["x tcp:1 localabstract:lantern"])))
        self.assertFalse(dt.released(released_state(helper_pids=["123"])))

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
        self.assertEqual(dt.MANUAL_WAIT, int(os.environ.get("LANTERN_MANUAL_WAIT", "3600")))

    def test_pair_items_need_pair(self):
        self.assertTrue(set(dt.PAIR) <= set(dt.ITEMS))

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


if __name__ == "__main__":
    unittest.main()
