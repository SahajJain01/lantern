#!/usr/bin/env python3
"""Lantern build: phone helper jar (javac + d8, no Gradle), then the Rust host.

    python3 tools/build.py helper   # host-JVM tests, then build/helper/lantern-helper.jar
    python3 tools/build.py all      # helper, then cargo build --release (host/)

Toolchain: ~/.local/opt/pd-toolchain (override with PD_TC): jdk/, build-tools/, platform/.
"""
import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
TC = Path(os.environ.get("PD_TC", Path.home() / ".local/opt/pd-toolchain"))
JDK = TC / "jdk"
BT = TC / "build-tools"
ANDROID_JAR = TC / "platform/android.jar"
FRAMEWORK_AIDL = TC / "platform/framework.aidl"
MIN_API = "35"

HELPER = ROOT / "android/helper"
VENDOR = HELPER / "vendor"
OUT = ROOT / "build/helper"
JAR = OUT / "lantern-helper.jar"
ASSET = ROOT / "host/lantern/assets/helper.jar"

# AIDL interfaces the vendored scrcpy wrappers need (paths relative to vendor/aidl).
AIDL = ["android/view/IDisplayWindowListener.aidl"]

# scrcpy's build generates this; the vendored util/IO.java references it.
BUILD_CONFIG = """package com.genymobile.scrcpy;

public final class BuildConfig {
  public static final boolean DEBUG = false;
  public static final String VERSION_NAME = "5.0.1";
}
"""

ENV = dict(os.environ, JAVA_HOME=str(JDK), PATH=f"{JDK}/bin:{os.environ['PATH']}")


def run(*args, cwd=None):
    args = [str(a) for a in args]
    print("+", " ".join(args), flush=True)
    subprocess.run(args, check=True, env=ENV, cwd=cwd)


def java_files(*dirs):
    return sorted(p for d in dirs if d.exists() for p in d.rglob("*.java"))


def fresh(d):
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)


def javac(sources, classes, classpath):
    run(JDK / "bin/javac", "--release", "11", "-encoding", "UTF-8", "-Xlint:-options",
        "-cp", os.pathsep.join(map(str, classpath)), "-d", classes, *sources)


def compile_helper():
    gen, classes = OUT / "gen", OUT / "classes"
    fresh(gen)
    fresh(classes)
    bc = gen / "com/genymobile/scrcpy/BuildConfig.java"
    bc.parent.mkdir(parents=True)
    bc.write_text(BUILD_CONFIG)
    for a in AIDL:
        run(BT / "aidl", f"-o{gen}", "-I.", "-p", FRAMEWORK_AIDL, a, cwd=VENDOR / "aidl")
    javac(java_files(VENDOR / "java", gen, HELPER / "src"), classes, [ANDROID_JAR])
    return classes


def run_tests(classes):
    """Plain-main test runners on the host JVM. android.jar is NOT on the runtime classpath, so a
    test that touches an Android class fails with NoClassDefFoundError."""
    tests = OUT / "test-classes"
    fresh(tests)
    sources = java_files(HELPER / "test")
    javac(sources, tests, [classes])
    golden = ROOT / "host/proto/golden"
    for src in sources:
        if not src.name.endswith("Test.java"):
            continue
        cls = ".".join(src.relative_to(HELPER / "test").with_suffix("").parts)
        run(JDK / "bin/java", f"-Dlantern.golden={golden}",
            "-cp", os.pathsep.join([str(tests), str(classes)]), cls, cwd=ROOT)


def dex(classes):
    if JAR.exists():
        JAR.unlink()
    run(BT / "d8", "--release", "--lib", ANDROID_JAR, "--min-api", MIN_API,
        "--output", JAR, *sorted(classes.rglob("*.class")))
    ASSET.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(JAR, ASSET)
    digest = hashlib.sha256(JAR.read_bytes()).hexdigest()
    print(f"BUILT {JAR.relative_to(ROOT)} bytes={JAR.stat().st_size} sha256={digest}")
    print(f"COPIED {ASSET.relative_to(ROOT)}")


def build_helper():
    classes = compile_helper()
    run_tests(classes)
    dex(classes)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("target", choices=["helper", "all"])
    target = ap.parse_args().target
    try:
        build_helper()
        if target == "all":
            run("cargo", "build", "--release", cwd=ROOT / "host")
    except subprocess.CalledProcessError as e:
        print(f"build failed: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
