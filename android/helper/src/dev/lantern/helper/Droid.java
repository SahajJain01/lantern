package dev.lantern.helper;

import android.os.Looper;
import android.system.Os;
import android.util.Log;

import com.genymobile.scrcpy.Workarounds;

import java.lang.reflect.Field;

/** Android-only process plumbing for app_process entry points. Never load from host tests. */
final class Droid {
    static final String TAG = "lantern";

    private Droid() {
    }

    /** app_process has no main Looper; scrcpy's wrappers need one plus {@link Workarounds}. */
    static void prepare() {
        try {
            Looper.prepare();
            Field main = Looper.class.getDeclaredField("sMainLooper");
            main.setAccessible(true);
            main.set(null, Looper.myLooper());
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException("main looper setup", e);
        }
        Workarounds.apply();
    }

    /** New session, so killing the parent's process group/terminal doesn't take us with it. */
    static void detach() {
        try {
            Os.setsid();
        } catch (Exception e) {
            log("setsid failed: " + e);
        }
    }

    /** Points fd 1 at /dev/null so later writes can't SIGPIPE us once the parent is gone. */
    static void silenceStdout() {
        try {
            java.io.FileDescriptor devNull = Os.open("/dev/null", android.system.OsConstants.O_WRONLY, 0);
            Os.dup2(devNull, 1);
            Os.close(devNull);
        } catch (Exception e) {
            log("silence stdout failed: " + e);
        }
    }

    static int pid() {
        return android.os.Process.myPid();
    }

    static void log(String msg) {
        Log.i(TAG, msg);
    }
}
