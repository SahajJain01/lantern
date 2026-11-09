package dev.lantern.helper;

import java.util.concurrent.Callable;

/**
 * Which activity to start for {@code pkg} in Android user {@code user}, how, and why not. Pure
 * policy over a {@link Backend} (host tests use a fake; the device uses {@link DeviceUsers}).
 *
 * <p>Routes:
 * <ul>
 * <li>{@link Route#DIRECT}: {@code IActivityManager.startActivityAsUser(…, user)}, like
 * {@code am start --user N}.</li>
 * <li>{@link Route#LAUNCHER_APPS}: for a profile of the current user (work profile, private space,
 * clone) whose admin set {@code no_debugging_features}. Android then refuses every shell-uid call
 * into that user ("Shell does not have permission to access user N"), but LauncherApps checks the
 * profile group instead, as it does for the phone's own launcher, so it can list and start the
 * profile's launcher activities.</li>
 * <li>{@link Route#LEGACY}: user 0 when the hidden user/package checks themselves fail (another
 * Android release): {@code getLaunchIntentForPackage}, as before user support existed.</li>
 * </ul>
 */
final class UserLaunch {
    private UserLaunch() {
    }

    /** What the phone knows about users and packages. No Android types, so host tests can fake it. */
    interface Backend {
        /** Every existing user id. May throw if the list can't be read. */
        int[] users();

        /** Running and unlocked (a RUNNING_LOCKED user can't start its apps yet). */
        boolean running(int user);

        /** True when this process is the adb shell uid (2000), the uid the debugging restriction bars. */
        boolean isShell();

        /** True when the user has DISALLOW_DEBUGGING_FEATURES, which bars the shell uid from it. */
        boolean debuggingBlocked(int user);

        boolean installed(String pkg, int user);

        /** The launcher activity's class name for {@code pkg} in {@code user}, or null. */
        String launcherActivity(String pkg, int user);

        /** LauncherApps' profile list: the current user's profile group (includes the current user). */
        int[] profiles();

        /** LauncherApps' first launcher activity class of {@code pkg} in {@code user}, or null. */
        String profileLauncherActivity(String pkg, int user);

        /** True when {@code user} is a managed (work) profile. False on any error. */
        boolean managedProfile(int user);
    }

    enum Route {
        DIRECT, LAUNCHER_APPS, LEGACY
    }

    /** A user-facing reason the app can't be opened. */
    static final class LaunchError extends Exception {
        LaunchError(String msg) {
            super(msg);
        }
    }

    static final class Target {
        final String pkg;
        /** Null for {@link Route#LEGACY} (the launch intent is resolved at start). */
        final String cls;
        final int user;
        final Route route;

        Target(String pkg, String cls, int user, Route route) {
            this.pkg = pkg;
            this.cls = cls;
            this.user = user;
            this.route = route;
        }

        String flatten() {
            return pkg + "/" + (cls != null ? cls : "?");
        }
    }

    /**
     * {@link #resolve} with a backend that may fail to build or to answer. For user 0 any such
     * failure (anything but a {@link LaunchError}) falls back to {@link Route#LEGACY}, so the personal
     * profile never depends on the hidden user/package APIs. For other users it is a clear error.
     */
    static Target resolveOrLegacy(String pkg, int user, Callable<Backend> backend) throws LaunchError {
        try {
            return resolve(pkg, user, backend.call());
        } catch (LaunchError e) {
            throw e;
        } catch (Exception e) {
            if (user == 0) {
                return new Target(pkg, null, 0, Route.LEGACY);
            }
            throw new LaunchError("could not check Android user " + user + " on this phone: " + e);
        }
    }

    /**
     * True when {@code user} is a managed (work) profile. User 0 never is (not asked); any failure
     * (building or asking the backend) is false: the work badge is cosmetic.
     */
    static boolean managedOrFalse(int user, Callable<Backend> backend) {
        if (user == 0) {
            return false;
        }
        try {
            return backend.call().managedProfile(user);
        } catch (Exception e) {
            return false;
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
            throw new LaunchError("Android user " + user + " is not running or not unlocked (is the work profile paused?)");
        }
        // The restriction bars the shell uid only (not root).
        if (b.isShell() && b.debuggingBlocked(user)) {
            return viaLauncherApps(pkg, user, b);
        }
        if (!b.installed(pkg, user)) {
            throw new LaunchError(pkg + " is not installed for Android user " + user);
        }
        String cls = b.launcherActivity(pkg, user);
        if (cls == null) {
            throw new LaunchError(pkg + " has no launcher activity for Android user " + user);
        }
        return new Target(pkg, cls, user, Route.DIRECT);
    }

    private static Target viaLauncherApps(String pkg, int user, Backend b) throws LaunchError {
        String why = "Android user " + user + " does not allow adb (its admin set no_debugging_features)";
        int[] profiles;
        try {
            profiles = b.profiles();
        } catch (RuntimeException e) {
            throw new LaunchError(why + ", and LauncherApps refused too: " + message(e));
        }
        if (!contains(profiles, user)) {
            throw new LaunchError(why + " and is not a profile of the current user, so LauncherApps"
                + " cannot open its apps either");
        }
        String cls;
        try {
            cls = b.profileLauncherActivity(pkg, user);
        } catch (RuntimeException e) {
            throw new LaunchError(why + ", and LauncherApps refused too: " + message(e));
        }
        if (cls == null) {
            throw new LaunchError(pkg + " is not installed (or has no launcher activity) for Android user " + user);
        }
        return new Target(pkg, cls, user, Route.LAUNCHER_APPS);
    }

    /** Message for a start that Android refused (e.g. a SecurityException the checks above missed). */
    static String startFailure(String pkg, int user, Throwable e) {
        return "Android refused to start " + pkg + " as user " + user + ": " + message(e);
    }

    private static String message(Throwable e) {
        return e.getMessage() != null ? e.getMessage() : e.toString();
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
