package dev.lantern.companion;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.function.Supplier;

/**
 * Host-JVM tests for {@link ImeWatchdog} (when the IME gives the keyboard back after the helper's
 * link is lost, with retry and fallback), {@link ImeState#linkUp} and {@link ImeOps} (op dispatch).
 * Android classes must never be loaded here.
 */
public final class ImeWatchdogTest {
    static final String PREV = "com.android.inputmethod.latin/.LatinIME";
    static final String OTHER = "com.other/.Keyboard";
    static final String LANTERN = "dev.lantern.companion/.LanternIme";
    static final Supplier<List<String>> ENABLED = () -> Arrays.asList(PREV, LANTERN);

    /** The keyboard the watchdog switches to now (prev or fallback), or null. */
    static String fire(ImeWatchdog w, long now, boolean current) {
        return fire(w, now, current, ENABLED);
    }

    static String fire(ImeWatchdog w, long now, boolean current, Supplier<List<String>> enabled) {
        ImeWatchdog.Action a = w.due(now, current, enabled);
        return a == null ? null : a.id;
    }

    static void firesAfterGrace() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(1000, PREV);
        ImeWatchdog.Action a = w.due(6000, true, ENABLED);
        eq(PREV, a.id, "fires at lostAt + grace");
        check(!a.fallback, "the original keyboard, not a fallback");
    }

    static void defaultGraceIsFiveSeconds() {
        ImeWatchdog w = new ImeWatchdog();
        w.linkLost(0, PREV);
        eq(null, fire(w, 4999, true), "not before 5 s");
        eq(PREV, fire(w, 5000, true), "at 5 s");
    }

    static void notBeforeGrace() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(1000, PREV);
        eq(null, fire(w, 1000, true), "at loss");
        eq(null, fire(w, 5999, true), "1 ms before grace");
    }

    static void linkUpCancels() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(1000, PREV);
        w.linkUp(); // a new session within 5 s must not lose its keyboard
        eq(null, fire(w, 10_000, true), "relinked: never fires");
        eq(null, fire(w, 100_000, true), "still never");
    }

    static void notWhenAnotherImeCurrent() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        eq(null, fire(w, 6000, false), "someone else's keyboard: leave it");
    }

    static void notWithoutPrev() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, "");
        eq(null, fire(w, 6000, true), "empty prev");
        ImeWatchdog n = new ImeWatchdog(5000);
        n.linkLost(0, null);
        eq(null, fire(n, 6000, true), "null prev");
        ImeWatchdog never = new ImeWatchdog(5000);
        eq(null, fire(never, 6000, true), "never linked, never lost");
    }

    /** Once the switch took (we are no longer current), it never fires again for that loss. */
    static void firesOnceWhenTheSwitchTakes() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        eq(PREV, fire(w, 6000, true), "first");
        eq(null, fire(w, 6500, true), "not again before the retry interval");
        eq(null, fire(w, 8000, false), "switched: done");
        eq(null, fire(w, 60_000, true), "still not (Lantern re-selected without a link)");
        // A later link and loss arms it again.
        w.linkUp();
        w.linkLost(70_000, PREV);
        eq(PREV, fire(w, 75_000, true), "fires for the next loss");
    }

    static void retryConstants() {
        eq(2000L, ImeWatchdog.RETRY_MS, "retry every 2 s");
        eq(5, ImeWatchdog.MAX_ATTEMPTS, "5 attempts at the original keyboard");
        eq(LANTERN, ImeWatchdog.LANTERN_ID, "Lantern's own ID");
    }

    /** A switch that did not take (threw, or silently ignored) is retried after RETRY_MS. */
    static void failureRearms() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        eq(PREV, fire(w, 5000, true), "attempt 1");
        eq(null, fire(w, 6999, true), "waits RETRY_MS");
        eq(PREV, fire(w, 7000, true), "attempt 2: still on the Lantern IME");
    }

    static void fallsBackAfterMaxAttempts() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        Supplier<List<String>> enabled = () -> Arrays.asList(LANTERN, PREV, OTHER);
        long t = 5000;
        for (int i = 1; i <= ImeWatchdog.MAX_ATTEMPTS; i++, t += ImeWatchdog.RETRY_MS) {
            ImeWatchdog.Action a = w.due(t, true, enabled);
            eq(PREV, a.id, "attempt " + i);
            check(!a.fallback, "attempt " + i + " is the original");
        }
        ImeWatchdog.Action fb = w.due(t, true, enabled);
        eq(OTHER, fb.id, "fallback skips Lantern and the failing original");
        check(fb.fallback, "marked as fallback");
    }

    static void fallsBackAtOnceWhenPrevNotEnabled() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV); // uninstalled or disabled since
        ImeWatchdog.Action a = w.due(5000, true, () -> Arrays.asList(LANTERN, OTHER));
        eq(OTHER, a.id, "first enabled that is not Lantern");
        check(a.fallback, "fallback");
    }

    static void noCandidatesGivesUp() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        ImeWatchdog.Action a = w.due(5000, true, () -> Collections.singletonList(LANTERN));
        check(a != null && a.id == null, "a give-up action");
        check(a.giveUp != null && !a.giveUp.isEmpty(), "with a reason to log");
        eq(null, w.due(7000, true, ENABLED), "stops");
        eq(null, w.due(60_000, true, ENABLED), "for good");
    }

    static void failedFallbackGivesUp() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        Supplier<List<String>> enabled = () -> Arrays.asList(OTHER, LANTERN);
        eq(OTHER, fire(w, 5000, true, enabled), "fallback at once");
        ImeWatchdog.Action a = w.due(7000, true, enabled);
        check(a != null && a.id == null && a.giveUp != null, "fallback did not take: give up");
        eq(null, w.due(9000, true, enabled), "stops");
    }

    static void unreadableEnabledListRetriesPrevThenGivesUp() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        Supplier<List<String>> broken = () -> {
            throw new IllegalStateException("no IMM");
        };
        long t = 5000;
        for (int i = 1; i <= ImeWatchdog.MAX_ATTEMPTS; i++, t += ImeWatchdog.RETRY_MS) {
            eq(PREV, fire(w, t, true, broken), "attempt " + i + " assumes prev is enabled");
        }
        ImeWatchdog.Action a = w.due(t, true, broken);
        check(a != null && a.id == null && a.giveUp != null, "no candidates known: give up");
    }

    static void enabledListReadOnlyWhenFiring() {
        int[] calls = {0};
        Supplier<List<String>> counting = () -> {
            calls[0]++;
            return Arrays.asList(PREV, LANTERN);
        };
        ImeWatchdog w = new ImeWatchdog(5000);
        w.due(1000, true, counting);
        w.linkLost(0, PREV);
        w.due(1000, true, counting);
        w.due(6000, false, counting);
        eq(0, calls[0], "not read while nothing is due");
        w.due(6000, true, counting);
        eq(1, calls[0], "read once when firing");
    }

    /** A relink drops the previous session's ime_prev, so a loss before the new one cannot restore it. */
    static void linkUpClearsStaleImePrev() {
        ImeState.prevIme = PREV;
        ImeState.linkUp();
        eq(null, ImeState.prevIme, "stale ime_prev cleared at linkUp");
        ImeState.WATCHDOG.linkLost(0, ImeState.prevIme); // lost before the new ime_prev arrived
        eq(null, fire(ImeState.WATCHDOG, 60_000, true), "nothing to restore");
        ImeState.linkUp();
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
            {"firesOnceWhenTheSwitchTakes", (Case) ImeWatchdogTest::firesOnceWhenTheSwitchTakes},
            {"retryConstants", (Case) ImeWatchdogTest::retryConstants},
            {"failureRearms", (Case) ImeWatchdogTest::failureRearms},
            {"fallsBackAfterMaxAttempts", (Case) ImeWatchdogTest::fallsBackAfterMaxAttempts},
            {"fallsBackAtOnceWhenPrevNotEnabled", (Case) ImeWatchdogTest::fallsBackAtOnceWhenPrevNotEnabled},
            {"noCandidatesGivesUp", (Case) ImeWatchdogTest::noCandidatesGivesUp},
            {"failedFallbackGivesUp", (Case) ImeWatchdogTest::failedFallbackGivesUp},
            {"unreadableEnabledListRetriesPrevThenGivesUp", (Case) ImeWatchdogTest::unreadableEnabledListRetriesPrevThenGivesUp},
            {"enabledListReadOnlyWhenFiring", (Case) ImeWatchdogTest::enabledListReadOnlyWhenFiring},
            {"linkUpClearsStaleImePrev", (Case) ImeWatchdogTest::linkUpClearsStaleImePrev},
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
