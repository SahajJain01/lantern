package dev.lantern.helper;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Host-JVM tests for {@link UserLaunch}: which user/app is launched, and the errors, via a fake. */
public final class UserLaunchTest {
    /** A phone with users 0 (running) and 10 (running) unless a test changes it. */
    static final class Fake implements UserLaunch.Backend {
        final List<Integer> users = new ArrayList<>(List.of(0, 10));
        final List<Integer> stopped = new ArrayList<>();
        final List<Integer> blocked = new ArrayList<>();
        final Map<String, String> launchers = new HashMap<>(); // "pkg:user" -> activity class
        final List<String> calls = new ArrayList<>();
        RuntimeException usersError;

        Fake app(String pkg, int user, String cls) {
            launchers.put(pkg + ":" + user, cls);
            return this;
        }

        @Override
        public int[] users() {
            calls.add("users");
            if (usersError != null) {
                throw usersError;
            }
            return users.stream().mapToInt(Integer::intValue).toArray();
        }

        @Override
        public boolean running(int user) {
            calls.add("running " + user);
            return !stopped.contains(user);
        }

        @Override
        public boolean debuggingBlocked(int user) {
            calls.add("blocked " + user);
            return blocked.contains(user);
        }

        @Override
        public boolean installed(String pkg, int user) {
            calls.add("installed " + pkg + " " + user);
            return launchers.containsKey(pkg + ":" + user);
        }

        @Override
        public String launcherActivity(String pkg, int user) {
            calls.add("launcher " + pkg + " " + user);
            if (blocked.contains(user) && shell) {
                // What Android does: the shell uid may not touch a no_debugging_features user.
                throw new SecurityException("Shell does not have permission to access user " + user);
            }
            String cls = launchers.get(pkg + ":" + user);
            return "none".equals(cls) ? null : cls;
        }

        boolean shell = true;
        final List<Integer> profileGroup = new ArrayList<>(List.of(0, 10));
        RuntimeException launcherAppsError;

        @Override
        public boolean isShell() {
            calls.add("isShell");
            return shell;
        }

        @Override
        public int[] profiles() {
            calls.add("profiles");
            if (launcherAppsError != null) {
                throw launcherAppsError;
            }
            return profileGroup.stream().mapToInt(Integer::intValue).toArray();
        }

        @Override
        public String profileLauncherActivity(String pkg, int user) {
            calls.add("la " + pkg + " " + user);
            if (launcherAppsError != null) {
                throw launcherAppsError;
            }
            String cls = launchers.get(pkg + ":" + user);
            return "none".equals(cls) ? null : cls;
        }
    }

    /** Every call throws, as when the hidden APIs differ on another Android release. */
    static final class Broken implements UserLaunch.Backend {
        private static RuntimeException no() {
            return new IllegalStateException("no android.os.IUserManager$Stub$Proxy.getUsers/3");
        }

        public int[] users() {
            throw no();
        }

        public boolean running(int user) {
            throw no();
        }

        public boolean isShell() {
            throw no();
        }

        public boolean debuggingBlocked(int user) {
            throw no();
        }

        public boolean installed(String pkg, int user) {
            throw no();
        }

        public String launcherActivity(String pkg, int user) {
            throw no();
        }

        public int[] profiles() {
            throw no();
        }

        public String profileLauncherActivity(String pkg, int user) {
            throw no();
        }
    }

    static void user0FallsBackToLegacyWhenHiddenApisFail() throws Exception {
        UserLaunch.Target t = UserLaunch.resolveOrLegacy("com.a", 0, Broken::new);
        eq(UserLaunch.Route.LEGACY, t.route, "route");
        eq(null, t.cls, "cls");
        eq(0, t.user, "user");
        // Backend construction failing (reflection) also falls back.
        t = UserLaunch.resolveOrLegacy("com.a", 0, () -> {
            throw new ClassNotFoundException("android.os.IUserManager$Stub");
        });
        eq(UserLaunch.Route.LEGACY, t.route, "route after ctor failure");
    }

    static void otherUsersDoNotFallBack() {
        String msg = fails(() -> UserLaunch.resolveOrLegacy("com.a", 10, Broken::new));
        contains(msg, "could not check Android user 10");
    }

    static void user0RealErrorsStillWin() {
        // A definite answer from the checks is not a hidden-API failure: no fallback.
        Fake f = new Fake();
        contains(fails(() -> UserLaunch.resolveOrLegacy("com.a", 0, () -> f)), "com.a is not installed for Android user 0");
    }

    static void blockedProfileUsesLauncherApps() throws Exception {
        Fake f = new Fake().app("com.a", 10, "com.a.WorkMain");
        f.blocked.add(10);
        UserLaunch.Target t = UserLaunch.resolve("com.a", 10, f);
        eq(UserLaunch.Route.LAUNCHER_APPS, t.route, "route");
        eq("com.a/com.a.WorkMain", t.flatten(), "target");
        // Never touches the package manager for a user it may not access.
        eq(false, f.calls.stream().anyMatch(c -> c.startsWith("installed") || c.startsWith("launcher")),
            "no PM calls: " + f.calls);
    }

    static void unblockedUsersStayDirect() throws Exception {
        Fake f = new Fake().app("com.a", 10, "com.a.Main");
        UserLaunch.Target t = UserLaunch.resolve("com.a", 10, f);
        eq(UserLaunch.Route.DIRECT, t.route, "route");
        eq(false, f.calls.contains("profiles"), "no LauncherApps: " + f.calls);
    }

    static void rootIgnoresTheDebuggingRestriction() throws Exception {
        Fake f = new Fake().app("com.a", 10, "com.a.Main");
        f.blocked.add(10);
        f.shell = false;
        eq(UserLaunch.Route.DIRECT, UserLaunch.resolve("com.a", 10, f).route, "route as root");
        eq(false, f.calls.contains("blocked 10"), "restriction not even read: " + f.calls);
    }

    static void blockedNonProfileIsAClearError() {
        Fake f = new Fake().app("com.a", 10, "com.a.Main");
        f.blocked.add(10);
        f.profileGroup.remove(Integer.valueOf(10));
        String msg = fails(() -> UserLaunch.resolve("com.a", 10, f));
        contains(msg, "no_debugging_features");
        contains(msg, "not a profile of the current user");
    }

    static void launcherAppsRefusalIsExplained() {
        Fake f = new Fake().app("com.a", 10, "com.a.Main");
        f.blocked.add(10);
        f.launcherAppsError = new SecurityException("Caller cannot access profile");
        String msg = fails(() -> UserLaunch.resolve("com.a", 10, f));
        contains(msg, "no_debugging_features");
        contains(msg, "LauncherApps refused too: Caller cannot access profile");
    }

    static void blockedProfileWithoutTheApp() {
        Fake f = new Fake().app("com.a", 0, "com.a.Main");
        f.blocked.add(10);
        contains(fails(() -> UserLaunch.resolve("com.a", 10, f)),
            "com.a is not installed (or has no launcher activity) for Android user 10");
    }

    static void resolvesTheActivityForThatUser() throws Exception {
        Fake f = new Fake().app("com.a", 0, "com.a.Main").app("com.a", 10, "com.a.WorkMain");
        eq("com.a/com.a.Main", UserLaunch.resolve("com.a", 0, f).flatten(), "user 0");
        UserLaunch.Target t = UserLaunch.resolve("com.a", 10, f);
        eq("com.a/com.a.WorkMain", t.flatten(), "user 10");
        eq(10, t.user, "target user");
        eq(true, f.calls.contains("launcher com.a 10"), "asked PM for user 10: " + f.calls);
    }

    static void missingUserIsAClearError() {
        Fake f = new Fake().app("com.a", 0, "com.a.Main");
        String msg = fails(() -> UserLaunch.resolve("com.a", 11, f));
        contains(msg, "Android user 11 does not exist");
        contains(msg, "users: 0, 10");
    }

    static void stoppedUserIsAClearError() {
        Fake f = new Fake().app("com.a", 10, "com.a.Main");
        f.stopped.add(10);
        contains(fails(() -> UserLaunch.resolve("com.a", 10, f)), "Android user 10 is not running");
    }

    static void notInstalledForThatUser() {
        Fake f = new Fake().app("com.a", 0, "com.a.Main");
        String msg = fails(() -> UserLaunch.resolve("com.a", 10, f));
        contains(msg, "com.a is not installed for Android user 10");
    }

    static void noLauncherActivity() {
        Fake f = new Fake().app("com.a", 10, "none");
        contains(fails(() -> UserLaunch.resolve("com.a", 10, f)), "com.a has no launcher activity for Android user 10");
    }

    static void userListFailureFallsBackToRunning() throws Exception {
        // If the user list can't be read, a running user is still accepted and a stopped one refused.
        Fake f = new Fake().app("com.a", 10, "com.a.Main");
        f.usersError = new SecurityException("no MANAGE_USERS");
        eq("com.a/com.a.Main", UserLaunch.resolve("com.a", 10, f).flatten(), "fallback");
        f.stopped.add(10);
        contains(fails(() -> UserLaunch.resolve("com.a", 10, f)), "Android user 10 is not running");
    }

    static void securityExceptionOnStartIsExplained() {
        String msg = UserLaunch.startFailure("com.a", 10,
            new SecurityException("Shell does not have permission to access user 10"));
        contains(msg, "Android refused to start com.a as user 10");
        contains(msg, "Shell does not have permission to access user 10");
    }

    interface Call {
        Object run() throws Exception;
    }

    private static String fails(Call c) {
        try {
            c.run();
        } catch (UserLaunch.LaunchError e) {
            return e.getMessage();
        } catch (Exception e) {
            throw new AssertionError("want LaunchError, got " + e);
        }
        throw new AssertionError("want LaunchError, got success");
    }

    private static void contains(String s, String part) {
        if (s == null || !s.contains(part)) {
            throw new AssertionError("want \"" + part + "\" in \"" + s + "\"");
        }
    }

    private static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"resolvesTheActivityForThatUser", (Case) UserLaunchTest::resolvesTheActivityForThatUser},
            {"missingUserIsAClearError", (Case) UserLaunchTest::missingUserIsAClearError},
            {"stoppedUserIsAClearError", (Case) UserLaunchTest::stoppedUserIsAClearError},
            {"user0FallsBackToLegacyWhenHiddenApisFail", (Case) UserLaunchTest::user0FallsBackToLegacyWhenHiddenApisFail},
            {"otherUsersDoNotFallBack", (Case) UserLaunchTest::otherUsersDoNotFallBack},
            {"user0RealErrorsStillWin", (Case) UserLaunchTest::user0RealErrorsStillWin},
            {"blockedProfileUsesLauncherApps", (Case) UserLaunchTest::blockedProfileUsesLauncherApps},
            {"unblockedUsersStayDirect", (Case) UserLaunchTest::unblockedUsersStayDirect},
            {"rootIgnoresTheDebuggingRestriction", (Case) UserLaunchTest::rootIgnoresTheDebuggingRestriction},
            {"blockedNonProfileIsAClearError", (Case) UserLaunchTest::blockedNonProfileIsAClearError},
            {"launcherAppsRefusalIsExplained", (Case) UserLaunchTest::launcherAppsRefusalIsExplained},
            {"blockedProfileWithoutTheApp", (Case) UserLaunchTest::blockedProfileWithoutTheApp},
            {"notInstalledForThatUser", (Case) UserLaunchTest::notInstalledForThatUser},
            {"noLauncherActivity", (Case) UserLaunchTest::noLauncherActivity},
            {"userListFailureFallsBackToRunning", (Case) UserLaunchTest::userListFailureFallsBackToRunning},
            {"securityExceptionOnStartIsExplained", (Case) UserLaunchTest::securityExceptionOnStartIsExplained},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   UserLaunchTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL UserLaunchTest." + c[0] + ": " + t);
            }
        }
        System.out.println("UserLaunchTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
