package dev.lantern.helper;

/**
 * Which activity to start for {@code pkg} in Android user {@code user}, and why not. Pure policy
 * over a {@link Backend} (host tests use a fake; the device uses {@link DeviceUsers}).
 */
final class UserLaunch {
    private UserLaunch() {
    }

    /** What the phone knows about users and packages. No Android types, so host tests can fake it. */
    interface Backend {
        /** Every existing user id. May throw if the list can't be read. */
        int[] users();

        boolean running(int user);

        /** True when the user has DISALLOW_DEBUGGING_FEATURES, which bars the shell uid from it. */
        boolean debuggingBlocked(int user);

        boolean installed(String pkg, int user);

        /** The launcher activity's class name for {@code pkg} in {@code user}, or null. */
        String launcherActivity(String pkg, int user);
    }

    /** A user-facing reason the app can't be opened. */
    static final class LaunchError extends Exception {
        LaunchError(String msg) {
            super(msg);
        }
    }

    static final class Target {
        final String pkg;
        final String cls;
        final int user;

        Target(String pkg, String cls, int user) {
            this.pkg = pkg;
            this.cls = cls;
            this.user = user;
        }

        String flatten() {
            return pkg + "/" + cls;
        }
    }

    static Target resolve(String pkg, int user, Backend b) throws LaunchError {
        int[] users = null;
        try {
            users = b.users();
        } catch (RuntimeException e) {
            // Can't list users: fall through to the running check, which also fails for a missing user.
        }
        if (users != null && !contains(users, user)) {
            throw new LaunchError("Android user " + user + " does not exist on the phone (users: " + join(users) + ")");
        }
        if (!b.running(user)) {
            throw new LaunchError("Android user " + user + " is not running (is the work profile paused?)");
        }
        if (b.debuggingBlocked(user)) {
            throw new LaunchError("Android user " + user + " does not allow adb: its admin set the"
                + " no_debugging_features restriction, so Lantern (running as adb shell) cannot open its apps");
        }
        if (!b.installed(pkg, user)) {
            throw new LaunchError(pkg + " is not installed for Android user " + user);
        }
        String cls = b.launcherActivity(pkg, user);
        if (cls == null) {
            throw new LaunchError(pkg + " has no launcher activity for Android user " + user);
        }
        return new Target(pkg, cls, user);
    }

    /** Message for a start that Android refused (e.g. a SecurityException the checks above missed). */
    static String startFailure(String pkg, int user, Throwable e) {
        return "Android refused to start " + pkg + " as user " + user + ": "
            + (e.getMessage() != null ? e.getMessage() : e.toString());
    }

    private static boolean contains(int[] a, int v) {
        for (int x : a) {
            if (x == v) {
                return true;
            }
        }
        return false;
    }

    private static String join(int[] a) {
        StringBuilder s = new StringBuilder();
        for (int x : a) {
            if (s.length() > 0) {
                s.append(", ");
            }
            s.append(x);
        }
        return s.toString();
    }
}
