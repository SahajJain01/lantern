"""Local, Gradle-free builds of throwaway probe APK and shell DEX jar."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import zipfile

ROOT = Path(__file__).resolve().parent
TC = Path(os.environ.get("PD_TC", Path.home()/".local/opt/pd-toolchain"))
JDK = TC / "jdk"
BT = TC / "build-tools"
PLATFORM = TC / "platform/android.jar"
OUT = ROOT / "out"
ENV = dict(os.environ, JAVA_HOME=str(JDK), PATH=f"{JDK}/bin:{os.environ['PATH']}")

def run(*args):
    subprocess.run(list(map(str, args)), check=True, env=ENV)

def javac(sources, classes):
    classes.mkdir(parents=True, exist_ok=True)
    run(JDK/"bin/javac", "--release", "11", "-encoding", "UTF-8", "-cp", PLATFORM,
        "-d", classes, *sources)

def dex(classes, output):
    output.mkdir(parents=True, exist_ok=True)
    run(BT/"d8", "--release", "--lib", PLATFORM, "--min-api", "30", "--output", output,
        *sorted(classes.rglob("*.class")))

def main():
    OUT.mkdir(exist_ok=True)
    # Clean generated trees only; signing key and raw phone evidence are separate.
    work = OUT/"work"
    if work.exists(): shutil.rmtree(work)
    work.mkdir()
    (work/"gen").mkdir()
    run(BT/"aapt2", "compile", "--dir", ROOT/"probe-app/res", "-o", work/"res.zip")
    run(BT/"aapt2", "link", "-I", PLATFORM, "--manifest", ROOT/"probe-app/AndroidManifest.xml",
        "--min-sdk-version", "30", "--target-sdk-version", "36", "--version-code", "1",
        "--version-name", "0.0.1-spike", "--java", work/"gen", "-o", work/"unsigned.apk", work/"res.zip")
    javac(sorted((ROOT/"probe-app/src").rglob("*.java")) + sorted((work/"gen").rglob("*.java")), work/"app-classes")
    dex(work/"app-classes", work/"app-dex")
    with zipfile.ZipFile(work/"unsigned.apk", "a", zipfile.ZIP_DEFLATED) as z:
        for f in sorted((work/"app-dex").glob("*.dex")): z.write(f, f.name)
    run(BT/"zipalign", "-f", "-p", "4", work/"unsigned.apk", work/"aligned.apk")
    key = ROOT/".local/probe.keystore"
    key.parent.mkdir(exist_ok=True, mode=0o700)
    if not key.exists():
        run(JDK/"bin/keytool", "-genkeypair", "-keystore", key, "-storepass", "lantern-probe",
            "-keypass", "lantern-probe", "-alias", "probe", "-keyalg", "RSA", "-keysize", "2048",
            "-validity", "365", "-dname", "CN=Lantern disposable probe")
        key.chmod(0o600)
    run(BT/"apksigner", "sign", "--ks", key, "--ks-pass", "pass:lantern-probe",
        "--out", OUT/"probe.apk", work/"aligned.apk")
    run(BT/"apksigner", "verify", "--verbose", OUT/"probe.apk")
    javac(sorted((ROOT/"probe-shell/src").rglob("*.java")), work/"shell-classes")
    dex(work/"shell-classes", work/"shell-dex")
    with zipfile.ZipFile(OUT/"probe-shell.jar", "w", zipfile.ZIP_DEFLATED) as z:
        for f in sorted((work/"shell-dex").glob("*.dex")): z.write(f, f.name)
    for name in ("probe.apk", "probe-shell.jar"):
        f=OUT/name
        print(f"BUILT {f} bytes={f.stat().st_size} sha256={hashlib.sha256(f.read_bytes()).hexdigest()}")

if __name__ == "__main__": main()
