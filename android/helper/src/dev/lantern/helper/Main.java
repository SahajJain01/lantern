package dev.lantern.helper;

import android.os.Build;

/** Helper entry point, run as the adb shell user via {@code app_process}. */
public final class Main {
    private Main() {
    }

    public static void main(String[] args) {
        String cmd = args.length > 0 ? args[0] : "";
        switch (cmd) {
            case "--version":
                System.out.println("lantern-helper " + Proto.VERSION + " sdk=" + Build.VERSION.SDK_INT);
                return;
            case "--ping":
                System.out.println("pong");
                return;
            case "--session-test":
                if (args.length != 2) {
                    break;
                }
                System.exit(sessionTest(Long.parseLong(args[1])));
                return;
            default:
                break;
        }
        System.err.println("usage: dev.lantern.helper.Main --version | --ping | --session-test <seconds>");
        System.exit(2);
    }

    /** Starts a session, holds it for {@code seconds}, closes it. Prints {@code pid=N} for kill tests. */
    private static int sessionTest(long seconds) {
        Droid.prepare();
        System.out.println("pid=" + Droid.pid());
        System.out.flush();
        Droid.log("session-test start pid=" + Droid.pid() + " seconds=" + seconds);
        try (Session s = Session.start(new ShellSettings(), new DevicePower(), Cleanup.spawnGuard(Droid::log))) {
            Droid.log("session started, journal=" + s.journal().toJson());
            System.out.println("session started");
            System.out.flush();
            Thread.sleep(seconds * 1000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        } catch (RuntimeException e) {
            Droid.log("session-test failed: " + e);
            e.printStackTrace();
            return 1;
        }
        Droid.log("session closed");
        System.out.println("session closed");
        return 0;
    }
}
