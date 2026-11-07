"""Device-scoped, bounded probe I/O. Raw snapshots are private."""
import contextlib
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import subprocess
import time

ROOT = Path(__file__).resolve().parent
SERIAL = "20e9318"
ADB = str(Path.home() / ".local/bin/adb")
SCRCPY = str(Path.home() / ".local/bin/scrcpy")
SCRCPY_SERVER = Path(SCRCPY).resolve().parent / "scrcpy-server"

def adb(*args, check=True, timeout=30):
    p = subprocess.run([ADB, "-s", SERIAL, *map(str, args)], capture_output=True,
                       text=True, timeout=timeout)
    if check and p.returncode:
        raise RuntimeError(f"adb {args}: {p.stdout.strip()} {p.stderr.strip()}")
    return p.stdout

def shell(*args, **kw):
    return adb("shell", shlex.join(map(str, args)), **kw)

def require_device():
    state = adb("get-state").strip()
    if state != "device":
        raise RuntimeError(f"{SERIAL} unavailable: {state}")
    return {k: shell("getprop", k).strip() for k in (
        "ro.product.model", "ro.build.version.sdk", "ro.build.version.release",
        "ro.build.fingerprint", "ro.lineage.version")}

def settings(namespace):
    data = {}
    for line in shell("settings", "--user", "0", "list", namespace).splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            data[k] = v
    return data

def snapshot():
    device = require_device()
    namespaces = {n: settings(n) for n in ("global", "secure", "system")}
    return {"serial": SERIAL, "device": device, "settings": namespaces,
            "packages": sorted(shell("pm", "list", "packages", "--user", "0").splitlines())}

def diff(a, b, prefix=""):
    changes = []
    for k in sorted(a.keys() | b.keys()):
        path = f"{prefix}.{k}" if prefix else k
        if k not in a:
            changes.append(f"+ {path}={b[k]!r}")
        elif k not in b:
            changes.append(f"- {path}={a[k]!r}")
        elif isinstance(a[k], dict) and isinstance(b[k], dict):
            changes.extend(diff(a[k], b[k], path))
        elif a[k] != b[k]:
            changes.append(f"~ {path}: {a[k]!r} -> {b[k]!r}")
    return changes

def save_private(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(value, f, indent=2, ensure_ascii=False)
        f.write("\n")

def parse_event(line):
    if "event=" not in line:
        return None
    fields = {}
    for m in re.finditer(r'(\w+)=("(?:\\.|[^"\\])*"|[^\s]+)', line):
        key, val = m.groups()
        try:
            fields[key] = json.loads(val)
        except json.JSONDecodeError:
            fields[key] = val
    return fields

def logcat_lines(timeout_s):
    p = subprocess.Popen([ADB, "-s", SERIAL, "logcat", "-v", "raw", "-T", "1",
                          "LanternProbe:I", "*:S"], stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL)
    assert p.stdout is not None
    sel = selectors.DefaultSelector()
    sel.register(p.stdout, selectors.EVENT_READ)
    end = time.monotonic() + timeout_s
    buf = b""
    try:
        while time.monotonic() < end:
            ready = sel.select(max(0, end - time.monotonic()))
            if not ready:
                break
            data = os.read(p.stdout.fileno(), 65536)
            if not data:
                raise RuntimeError("logcat ended (device disconnected?)")
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                event = parse_event(line.decode("utf-8", "replace"))
                if event:
                    yield event
    finally:
        sel.close()
        p.terminate()
        try:
            p.wait(timeout=3)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()

def shell_probe(*args):
    require_device()
    adb("push", ROOT / "out/probe-shell.jar", "/data/local/tmp/lantern-probe.jar")
    adb("push", SCRCPY_SERVER, "/data/local/tmp/lantern-probe-scrcpy.jar")
    command = "CLASSPATH=/data/local/tmp/lantern-probe.jar:/data/local/tmp/lantern-probe-scrcpy.jar app_process / dev.lantern.probeshell.Main "
    return subprocess.Popen([ADB, "-s", SERIAL, "shell", "-T", command + shlex.join(map(str,args))],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True, bufsize=1)
