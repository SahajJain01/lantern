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

    /** No original known (reboot before any ime_prev, or ""): fall back to the first other enabled keyboard. */
    static void noPrevFallsBackToFirstEnabled() {
        for (String prev : new String[] {"", null}) {
            ImeWatchdog w = new ImeWatchdog(5000);
            w.linkLost(0, prev);
            eq(null, fire(w, 4999, true, () -> Arrays.asList(LANTERN, PREV)), "not before the grace");
            ImeWatchdog.Action a = w.due(5000, true, () -> Arrays.asList(LANTERN, PREV));
            check(a != null, "acts without a known original (" + prev + ")");
            eq(PREV, a.id, "first enabled keyboard that is not Lantern's");
            check(a.fallback, "marked as fallback");
        }
        ImeWatchdog only = new ImeWatchdog(5000);
        only.linkLost(0, null);
        ImeWatchdog.Action a = only.due(5000, true, () -> Collections.singletonList(LANTERN));
        check(a != null && a.id == null && a.giveUp != null, "no candidate at all: give up with a reason");
        ImeWatchdog never = new ImeWatchdog(5000);
        eq(null, fire(never, 6000, true), "never linked, never lost, never created: nothing");
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

    /** In-memory stand-in for the companion's device-protected store. */
    static final class FakeStore implements PrevStore {
        String value;
        int puts;

        FakeStore(String value) {
            this.value = value;
        }

        @Override
        public String get() {
            return value;
        }

        @Override
        public void put(String id) {
            puts++;
            value = id;
        }

        @Override
        public void clear() {
            value = null;
        }
    }

    /**
     * ime_prev is persisted (it must survive a reboot); a relink only marks the in-memory copy
     * stale, so the persisted one stays the restore target until the new session's ime_prev
     * replaces it or a restore succeeded.
     */
    static void imePrevPersistsAcrossLinkUpUntilReplacedOrRestored() {
        FakeStore store = new FakeStore(null);
        PrevStore saved = ImeState.store;
        ImeState.store = store;
        try {
            ImeState.linkUp();
            eq(null, ImeState.restoreTarget(), "nothing known yet");
            ImeState.imePrev(PREV);
            eq(PREV, store.value, "persisted when received");
            eq(PREV, ImeState.restoreTarget(), "restore target");
            ImeState.linkUp();
            eq(null, ImeState.prevIme, "in-memory copy dropped at linkUp (stale)");
            eq(PREV, store.value, "persisted copy kept at linkUp");
            eq(PREV, ImeState.restoreTarget(), "a loss before the new ime_prev still restores a keyboard");
            ImeState.imePrev("");
            eq(PREV, store.value, "an empty ime_prev never erases a known original");
            ImeState.imePrev(OTHER);
            eq(OTHER, store.value, "a new ime_prev replaces it");
            eq(OTHER, ImeState.restoreTarget(), "fresh value wins");
            ImeState.restored();
            eq(null, store.value, "cleared once a restore took");
            eq(null, ImeState.restoreTarget(), "nothing left");
        } finally {
            ImeState.store = saved;
            ImeState.linkUp();
        }
    }

    /** A broken store never breaks the link or the watchdog. */
    static void brokenStoreIsHarmless() {
        PrevStore saved = ImeState.store;
        ImeState.store = new PrevStore() {
            @Override
            public String get() {
                throw new IllegalStateException("locked");
            }

            @Override
            public void put(String id) {
                throw new IllegalStateException("locked");
            }

            @Override
            public void clear() {
                throw new IllegalStateException("locked");
            }
        };
        try {
            ImeState.imePrev(PREV);
            eq(PREV, ImeState.restoreTarget(), "the in-memory copy still works");
            ImeState.linkUp();
            eq(null, ImeState.restoreTarget(), "unreadable store: unknown (the watchdog falls back)");
            ImeState.restored();
        } finally {
            ImeState.store = saved;
            ImeState.linkUp();
        }
    }

    /** After a reboot the IME is created with no link: arm, and restore the persisted keyboard after the grace. */
    static void createdWithNoLinkArmsWithPersistedPrev() {
        FakeStore store = new FakeStore(PREV);
        PrevStore saved = ImeState.store;
        ImeState.store = store;
        ImeState.prevIme = null; // a fresh process: memory is empty
        try {
            ImeWatchdog w = new ImeWatchdog(5000);
            check(w.createdUnlinked(1000, ImeState.restoreTarget()), "armed");
            eq(null, fire(w, 5999, true), "not before the grace");
            ImeWatchdog.Action a = w.due(6000, true, ENABLED);
            eq(PREV, a.id, "the persisted original");
            check(!a.fallback, "not a fallback");
        } finally {
            ImeState.store = saved;
        }
    }

    static void createdWithNoLinkAndNoPrevFallsBack() {
        ImeWatchdog w = new ImeWatchdog(5000);
        check(w.createdUnlinked(0, null), "armed without a known original");
        ImeWatchdog.Action a = w.due(5000, true, () -> Arrays.asList(LANTERN, OTHER));
        eq(OTHER, a.id, "fallback candidate");
        check(a.fallback, "fallback");
    }

    static void createdWhileLinkedDoesNothing() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        check(!w.createdUnlinked(0, PREV), "a linked session's IME (e.g. recreated for another display) is not armed");
        eq(null, fire(w, 60_000, true), "never fires");
    }

    static void createdUnlinkedThenLinkWithinGraceCancels() {
        ImeWatchdog w = new ImeWatchdog(5000);
        check(w.createdUnlinked(0, PREV), "armed");
        w.linkUp(); // the IME was bound a moment before the companion processed the hello reply
        eq(null, fire(w, 5000, true), "cancelled");
        eq(null, fire(w, 60_000, true), "for good");
    }

    /** A recreated IME (display move) during a loss keeps the loss's progress. */
    static void createdUnlinkedKeepsAnOngoingLoss() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkUp();
        w.linkLost(0, PREV);
        eq(PREV, fire(w, 5000, true), "attempt 1");
        check(!w.createdUnlinked(6000, OTHER), "already armed: not reset");
        eq(PREV, fire(w, 7000, true), "attempt 2 keeps the original and the retry timing");
    }

    /** When a switch took, due() says so once (so the persisted original can be cleared). */
    static void switchThatTookReportsDone() {
        ImeWatchdog w = new ImeWatchdog(5000);
        w.linkLost(0, PREV);
        eq(PREV, fire(w, 5000, true), "switch");
        ImeWatchdog.Action done = w.due(7000, false, ENABLED);
        check(done != null && done.done && done.id == null && done.giveUp == null, "done");
        eq(null, w.due(9000, false, ENABLED), "only once");
        ImeWatchdog idle = new ImeWatchdog(5000);
        idle.linkLost(0, PREV);
        eq(null, idle.due(6000, false, ENABLED), "another keyboard before any switch: not done, stays armed");
        eq(PREV, fire(idle, 6500, true), "still armed");
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
            {"noPrevFallsBackToFirstEnabled", (Case) ImeWatchdogTest::noPrevFallsBackToFirstEnabled},
            {"firesOnceWhenTheSwitchTakes", (Case) ImeWatchdogTest::firesOnceWhenTheSwitchTakes},
            {"retryConstants", (Case) ImeWatchdogTest::retryConstants},
            {"failureRearms", (Case) ImeWatchdogTest::failureRearms},
            {"fallsBackAfterMaxAttempts", (Case) ImeWatchdogTest::fallsBackAfterMaxAttempts},
            {"fallsBackAtOnceWhenPrevNotEnabled", (Case) ImeWatchdogTest::fallsBackAtOnceWhenPrevNotEnabled},
            {"noCandidatesGivesUp", (Case) ImeWatchdogTest::noCandidatesGivesUp},
            {"failedFallbackGivesUp", (Case) ImeWatchdogTest::failedFallbackGivesUp},
            {"unreadableEnabledListRetriesPrevThenGivesUp", (Case) ImeWatchdogTest::unreadableEnabledListRetriesPrevThenGivesUp},
            {"enabledListReadOnlyWhenFiring", (Case) ImeWatchdogTest::enabledListReadOnlyWhenFiring},
            {"imePrevPersistsAcrossLinkUpUntilReplacedOrRestored", (Case) ImeWatchdogTest::imePrevPersistsAcrossLinkUpUntilReplacedOrRestored},
            {"brokenStoreIsHarmless", (Case) ImeWatchdogTest::brokenStoreIsHarmless},
            {"createdWithNoLinkArmsWithPersistedPrev", (Case) ImeWatchdogTest::createdWithNoLinkArmsWithPersistedPrev},
            {"createdWithNoLinkAndNoPrevFallsBack", (Case) ImeWatchdogTest::createdWithNoLinkAndNoPrevFallsBack},
            {"createdWhileLinkedDoesNothing", (Case) ImeWatchdogTest::createdWhileLinkedDoesNothing},
            {"createdUnlinkedThenLinkWithinGraceCancels", (Case) ImeWatchdogTest::createdUnlinkedThenLinkWithinGraceCancels},
            {"createdUnlinkedKeepsAnOngoingLoss", (Case) ImeWatchdogTest::createdUnlinkedKeepsAnOngoingLoss},
            {"switchThatTookReportsDone", (Case) ImeWatchdogTest::switchThatTookReportsDone},
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
