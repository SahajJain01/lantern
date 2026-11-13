package dev.lantern.helper;

/** Host-JVM tests for {@link FocusProbe}: display 0's focus from {@code dumpsys window displays}. Pure. */
public final class FocusProbeTest {
    /**
     * As in the spike (Motorola, Lantern session live): display 0 holds the PIN pad
     * ({@code NotificationShade}) while a Lantern display holds Windows App.
     */
    static String spikeDump(String display0Focus) {
        return "WINDOW MANAGER DISPLAY CONTENTS (dumpsys window displays)\n"
            + "  Display: mDisplayId=0 (organized) rootTasks=[Task{1 #1 type=home}]\n"
            + "    init=1220x2712 480dpi mMinSizeOfResizeableTaskDp=220 cur=1220x2712 app=1220x2552 rng=1220x1108-2552x2440\n"
            + "    deferred=false mLayoutNeeded=false mTouchExcludeRegion=SkRegion((0,0,1220,2712))\n"
            + "    mCurrentFocus=Window{" + display0Focus + "}\n"
            + "    mFocusedApp=ActivityRecord{9f1 u0 com.motorola.launcher3/com.android.searchlauncher.SearchLauncher t3}\n"
            + "  Display: mDisplayId=12 (organized) rootTasks=[Task{88 #88 type=standard}]\n"
            + "    init=1280x800 160dpi cur=1280x800 app=1280x800 rng=800x800-1280x1280\n"
            + "    mCurrentFocus=Window{7c2d0e1 u0 com.microsoft.rdc.androidx/com.microsoft.a3rdc.ui.activities.HomeActivity}\n"
            + "    mFocusedApp=ActivityRecord{5aa u0 com.microsoft.rdc.androidx/com.microsoft.a3rdc.ui.activities.HomeActivity t88}\n";
    }

    static void parsesDisplay0Focus() {
        String dump = spikeDump("3e5a8b1 u0 NotificationShade");
        eq("NotificationShade", FocusProbe.focusOfDisplay(dump, 0), "display 0 focus");
        eq("com.microsoft.rdc.androidx/com.microsoft.a3rdc.ui.activities.HomeActivity",
            FocusProbe.focusOfDisplay(dump, 12), "Lantern display focus");
        eq(null, FocusProbe.focusOfDisplay(dump, 1), "display 1 is not display 12");
        eq(null, FocusProbe.focusOfDisplay(dump, 3), "no such display");
        eq(null, FocusProbe.focusOfDisplay(null, 0), "null dump");
        eq(null, FocusProbe.focusOfDisplay(dump.replace("mCurrentFocus=Window{3e5a8b1 u0 NotificationShade}",
            "mCurrentFocus=null"), 0), "no focus on display 0");
        String dream = spikeDump("b44 u0 com.google.android.deskclock/com.android.deskclock.screensaver.DreamActivity");
        eq("com.google.android.deskclock/com.android.deskclock.screensaver.DreamActivity",
            FocusProbe.focusOfDisplay(dream, 0), "screensaver focus");
    }

    static void keyguardTitles() {
        eq(true, FocusProbe.isKeyguard("NotificationShade"), "NotificationShade");
        eq(true, FocusProbe.isKeyguard("Keyguard"), "Keyguard");
        eq(true, FocusProbe.isKeyguard("KeyguardBouncer"), "Bouncer");
        eq(false, FocusProbe.isKeyguard("com.google.android.deskclock/com.android.deskclock.screensaver.DreamActivity"),
            "DreamActivity");
        eq(false, FocusProbe.isKeyguard("com.microsoft.rdc.androidx/com.microsoft.a3rdc.ui.activities.HomeActivity"),
            "an app");
        eq(false, FocusProbe.isKeyguard(null), "null");
        eq(false, FocusProbe.isKeyguard("com.evil/.KeyguardActivity"), "app activity named Keyguard");
        eq(false, FocusProbe.isKeyguard("com.x.keyguardtheme/com.x.NotificationShadeMain"), "app activity named NotificationShade");
    }

    static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"parsesDisplay0Focus", (Case) FocusProbeTest::parsesDisplay0Focus},
            {"keyguardTitles", (Case) FocusProbeTest::keyguardTitles},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   FocusProbeTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL FocusProbeTest." + c[0] + ": " + t);
            }
        }
        System.out.println("FocusProbeTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
