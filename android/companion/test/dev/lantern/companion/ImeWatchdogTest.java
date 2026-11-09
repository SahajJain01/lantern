package dev.lantern.companion;

import java.util.ArrayList;
import java.util.List;

/**
 * Host-JVM tests for {@link ImeWatchdog} (when the IME gives the keyboard back after the helper's
 * link is lost) and {@link ImeOps} (op dispatch). Android classes must never be loaded here.
 */
public final class ImeWatchdogTest {
    static final String PREV = "com.android.inputmethod.latin/.LatinIME";

    static void firesAfterGrace() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(1000, PREV);
        eq(PREV, w.due(6000, true), "fires at lostAt + grace");
    }

    static void defaultGraceIsFiveSeconds() {
        ImeWatchdog w = new ImeWatchdog();
        w.linkLost(0, PREV);
        eq(null, w.due(4999, true), "not before 5 s");
        eq(PREV, w.due(5000, true), "at 5 s");
    }

    static void notBeforeGrace() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(1000, PREV);
        eq(null, w.due(1000, true), "at loss");
        eq(null, w.due(5999, true), "1 ms before grace");
    }

    static void linkUpCancels() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(1000, PREV);
        w.linkUp(); // a new session within 5 s must not lose its keyboard
        eq(null, w.due(10_000, true), "relinked: never fires");
        eq(null, w.due(100_000, true), "still never");
    }

    static void notWhenAnotherImeCurrent() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        eq(null, w.due(6000, false), "someone else's keyboard: leave it");
    }

    static void notWithoutPrev() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, "");
        eq(null, w.due(6000, true), "empty prev");
        ImeWatchdog n = new ImeWatchdog(5000);
        n.linkLost(0, null);
        eq(null, n.due(6000, true), "null prev");
        ImeWatchdog never = new ImeWatchdog(5000);
        eq(null, never.due(6000, true), "never linked, never lost");
    }

    static void firesOnce() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        eq(PREV, w.due(6000, true), "first");
        eq(null, w.due(6500, true), "not again for the same loss");
        eq(null, w.due(60_000, true), "still not");
        // A later link and loss arms it again.
        w.linkUp();
        w.linkLost(70_000, PREV);
        eq(PREV, w.due(75_000, true), "fires for the next loss");
    }

    static void opsDispatch() {
        List<String> calls = new ArrayList<>();
        ImeOps.Target t = new ImeOps.Target() {
            @Override
            public boolean commit(String text) {
                calls.add("commit " + text);
                return true;
            }

            @Override
            public boolean compose(String text) {
                calls.add("compose " + text);
                return true;
            }

            @Override
            public boolean finish() {
                calls.add("finish");
                return false;
            }
        };
        check(ImeOps.apply(t, "commit", "hi"), "commit ok");
        check(ImeOps.apply(t, "compose", ""), "compose ok");
        check(!ImeOps.apply(t, "finish", ""), "finish result passed through");
        check(!ImeOps.apply(t, "delete", "x"), "unknown op is not ok");
        check(!ImeOps.apply(null, "commit", "x"), "no input connection is not ok");
        eq("[commit hi, compose , finish]", calls.toString(), "calls");
        eq("ime op=commit ok=true len=2", ImeOps.logLine("commit", true, "hi"), "log line");
        eq("ime op=compose ok=false len=0", ImeOps.logLine("compose", false, null), "log line, null text");
    }

    static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"firesAfterGrace", (Case) ImeWatchdogTest::firesAfterGrace},
            {"defaultGraceIsFiveSeconds", (Case) ImeWatchdogTest::defaultGraceIsFiveSeconds},
            {"notBeforeGrace", (Case) ImeWatchdogTest::notBeforeGrace},
            {"linkUpCancels", (Case) ImeWatchdogTest::linkUpCancels},
            {"notWhenAnotherImeCurrent", (Case) ImeWatchdogTest::notWhenAnotherImeCurrent},
            {"notWithoutPrev", (Case) ImeWatchdogTest::notWithoutPrev},
            {"firesOnce", (Case) ImeWatchdogTest::firesOnce},
            {"opsDispatch", (Case) ImeWatchdogTest::opsDispatch},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   ImeWatchdogTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL ImeWatchdogTest." + c[0] + ": " + t);
            }
        }
        System.out.println("ImeWatchdogTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
