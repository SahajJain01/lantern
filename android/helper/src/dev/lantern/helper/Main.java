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
            default:
                System.err.println("usage: dev.lantern.helper.Main --version | --ping");
                System.exit(2);
        }
    }
}
