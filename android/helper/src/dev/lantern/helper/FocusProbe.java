package dev.lantern.helper;

import java.io.IOException;

/**
 * Which window has the focus on a display, from {@code dumpsys window displays}. The unlock types
 * its digits only while display 0's focus is the keyguard (spike: {@code NotificationShade} once
 * the PIN pad is up; {@code DreamActivity} while the charging screensaver still runs). Pure apart
 * from {@link #device}, so host tests can drive it.
 */
final class FocusProbe {
    /** How long {@code dumpsys window displays} may take. */
    static final long DUMP_TIMEOUT_MS = 3000;

    /** The text of {@code dumpsys window displays}. */
    interface Source {
        String dump() throws IOException;
    }

    private FocusProbe() {
    }

    /** Runs {@code dumpsys window displays}, killed after {@link #DUMP_TIMEOUT_MS}. */
    static Source device() {
        return () -> LockState.dumpsys(DUMP_TIMEOUT_MS, "window", "displays");
    }

    /**
     * The title in {@code mCurrentFocus=Window{<hash> u<N> <title>}} within the
     * {@code Display: mDisplayId=<display>} section, or null (no such display, no focus).
     */
    static String focusOfDisplay(String dumpsysWindowDisplays, int display) {
        if (dumpsysWindowDisplays == null) {
            return null;
        }
        boolean inside = false;
        for (String line : dumpsysWindowDisplays.split("\n")) {
            String t = line.trim();
            if (t.startsWith("Display: mDisplayId=")) {
                inside = displayId(t.substring("Display: mDisplayId=".length())) == display;
                continue;
            }
            if (inside && t.startsWith("mCurrentFocus=")) {
                return title(t.substring("mCurrentFocus=".length()));
            }
        }
        return null;
    }

    /** The leading decimal number of {@code s}, or -1. */
    private static int displayId(String s) {
        int end = 0;
        while (end < s.length() && end < 9 && Character.isDigit(s.charAt(end))) {
            end++;
        }
        return end == 0 ? -1 : Integer.parseInt(s.substring(0, end));
    }

    /** {@code Window{3e5a8b1 u0 NotificationShade}} to {@code NotificationShade}; else null. */
    private static String title(String v) {
        if (!v.startsWith("Window{") || !v.endsWith("}")) {
            return null;
        }
        String[] parts = v.substring("Window{".length(), v.length() - 1).split(" ", 3);
        return parts.length == 3 && !parts[2].isEmpty() ? parts[2] : null;
    }

    /** The keyguard (PIN pad) has the focus: {@code NotificationShade}, {@code Keyguard} or {@code Bouncer}. */
    static boolean isKeyguard(String title) {
        // System windows have no "pkg/cls" title. An app activity shown over the lock screen does,
        // and must never pass even if its class name contains one of these words.
        return title != null && title.indexOf('/') < 0
            && (title.contains("NotificationShade") || title.contains("Keyguard") || title.contains("Bouncer"));
    }
}
