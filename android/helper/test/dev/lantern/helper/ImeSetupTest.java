package dev.lantern.helper;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/** Host-JVM tests for {@link ImeSetup}: the enabled-IME list edit, the session changes, the relay. */
public final class ImeSetupTest {
    static final String ID = "dev.lantern.companion/.LanternIme";
    static final String LATIN = "com.android.inputmethod.latin/.LatinIME";

    static void addsOnce() {
        String once = ImeSetup.withEnabled(LATIN, ID);
        eq(LATIN + ":" + ID, once, "appended");
        eq(once, ImeSetup.withEnabled(once, ID), "second call leaves it unchanged");
    }

    static void keepsSubtypes() {
        String list = LATIN + ";12345;67890:com.other/.Ime;1";
        eq(list + ":" + ID, ImeSetup.withEnabled(list, ID), "subtypes kept, id appended");
        String ours = list + ":" + ID + ";424242";
        eq(ours, ImeSetup.withEnabled(ours, ID), "our entry with a subtype counts as present");
        // A component that merely starts with ours is a different IME.
        String prefix = LATIN + ":" + ID + "Fake";
        eq(prefix + ":" + ID, ImeSetup.withEnabled(prefix, ID), "prefix is not a match");
    }

    static void nullOrEmptyList() {
        eq(ID, ImeSetup.withEnabled(null, ID), "null list");
        eq(ID, ImeSetup.withEnabled("", ID), "empty list");
    }

    static void alreadyPresentUnchanged() {
        String list = ID + ":" + LATIN;
        eq(list, ImeSetup.withEnabled(list, ID), "first entry present: unchanged");
    }

    static void changesEnableThenSelect() {
        eq(ID, ImeSetup.ID, "ID");
        List<Change> c = ImeSetup.changes();
        eq(2, c.size(), "two changes");
        eq("secure", c.get(0).ns, "first ns");
        eq("enabled_input_methods", c.get(0).key, "first key");
        eq(LATIN + ":" + ID, c.get(0).next(LATIN), "enable appends");
        eq(ID, c.get(0).next(null), "enable from absent");
        eq("secure", c.get(1).ns, "second ns");
        eq("default_input_method", c.get(1).key, "second key");
        eq(ID, c.get(1).next(LATIN), "select ours");
        // Applied through selectIme (the shell's `ime set`), not a raw settings put.
        List<String> calls = new ArrayList<>();
        c.get(1).apply(new SettingsBackend() {
            @Override
            public String get(String ns, String key) {
                return null;
            }

            @Override
            public void put(String ns, String key, String value) {
                calls.add("put " + key);
            }

            @Override
            public void delete(String ns, String key) {
                calls.add("delete " + key);
            }

            @Override
            public void selectIme(String id) {
                calls.add("ime " + id);
            }
        }, ID);
        eq(Collections.singletonList("ime " + ID), calls, "selectIme");
    }

    static void prevImeFromJournal() {
        Journal j = new Journal(java.util.Arrays.asList(
            new Journal.Entry("global", "stay_on_while_plugged_in", "0", "7"),
            new Journal.Entry("secure", "enabled_input_methods", LATIN, LATIN + ":" + ID),
            new Journal.Entry("secure", "default_input_method", LATIN, ID)));
        eq(LATIN, ImeSetup.prevIme(j), "the original keyboard");
        eq("", ImeSetup.prevIme(new Journal(Collections.singletonList(
            new Journal.Entry("global", "stay_on_while_plugged_in", "0", "7")))), "no IME entry");
        eq("", ImeSetup.prevIme(new Journal(Collections.singletonList(
            new Journal.Entry("secure", "default_input_method", null, ID)))), "absent original");
        eq("", ImeSetup.prevIme(new Journal(Collections.singletonList(
            new Journal.Entry("secure", "default_input_method", ID, ID)))), "Lantern's own is never restored to");
    }

    /** Settings whose {@code ime set} is refused (Ruling 15). */
    static final class RefusingIme extends SessionTest.FakeSettings {
        String refuseRestoreOf; // a put of this value throws (a failing rollback)

        @Override
        public void selectIme(String id) {
            writes.add("selectIme " + id);
            throw new IllegalStateException("ime set refused");
        }

        @Override
        public void put(String ns, String key, String v) {
            if (v != null && v.equals(refuseRestoreOf)) {
                throw new IllegalStateException("put refused");
            }
            super.put(ns, key, v);
        }
    }

    /** A starter like the device's: a fresh guard per attempt; records guard and change-list events. */
    static final class RecordingStarter implements ImeSetup.Starter {
        final SettingsBackend settings;
        final List<String> events = new ArrayList<>();
        int guards;

        RecordingStarter(SettingsBackend settings) {
            this.settings = settings;
        }

        @Override
        public Session start(List<Change> changes) {
            int n = ++guards;
            events.add("start " + changes.size());
            return Session.start(settings, new SessionTest.FakePower(), new Session.Guard() {
                @Override
                public void arm(String journalJson) {
                    events.add("arm " + n);
                }

                @Override
                public void disarm() {
                    events.add("disarm " + n);
                }
            }, JournalStore.none(), changes);
        }
    }

    static RefusingIme phoneWithLatin() {
        RefusingIme s = new RefusingIme();
        s.values.put("global/stay_on_while_plugged_in", "15");
        s.values.put("secure/enabled_input_methods", LATIN);
        s.values.put("secure/default_input_method", LATIN);
        return s;
    }

    static void imeSwitchFailureFallsBackToStayOn() {
        RefusingIme s = phoneWithLatin();
        RecordingStarter st = new RecordingStarter(s);
        List<String> logs = new ArrayList<>();
        ImeSetup.Started r = ImeSetup.start(true, st, logs::add);
        check(!r.imeSelected, "keyboard not selected");
        eq("keyboard switch failed: ime set refused", r.imeFailure, "failure reason");
        // The first attempt rolled back and disarmed its Cleanup before the second was spawned.
        eq("[start 3, arm 1, disarm 1, start 1, arm 2]", st.events.toString(), "guard order");
        eq(1, r.session.journal().entries().size(), "STAY_ON only");
        eq("7", s.get("global", "stay_on_while_plugged_in"), "stay-on set");
        eq(LATIN, s.get("secure", "enabled_input_methods"), "enabled list restored");
        eq(LATIN, s.get("secure", "default_input_method"), "keyboard untouched");
        check(logs.toString().contains("ime set refused"), "failure logged: " + logs);
        r.session.close();
        eq("15", s.get("global", "stay_on_while_plugged_in"), "restored at the end");
        eq("disarm 2", st.events.get(st.events.size() - 1), "second guard disarmed at the end");
    }

    static void noRetryWhenRollbackFailed() {
        RefusingIme s = phoneWithLatin();
        s.refuseRestoreOf = LATIN; // restoring the enabled list fails: Cleanup owns the phone now
        RecordingStarter st = new RecordingStarter(s);
        try {
            ImeSetup.start(true, st, m -> { });
            throw new AssertionError("want the start failure rethrown");
        } catch (IllegalStateException e) {
            eq("ime set refused", e.getMessage(), "original failure");
        }
        eq("[start 3, arm 1]", st.events.toString(), "no second attempt, first guard left armed");
    }

    static void imeSelectedWhenItWorks() {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        s.values.put("secure/default_input_method", LATIN);
        RecordingStarter st = new RecordingStarter(s);
        ImeSetup.Started r = ImeSetup.start(true, st, m -> { });
        check(r.imeSelected, "selected");
        eq(null, r.imeFailure, "no failure");
        eq("[start 3, arm 1]", st.events.toString(), "one attempt");
        eq(ID, s.get("secure", "default_input_method"), "ours");
        r.session.close();
    }

    static void notLinkedIsStayOnOnly() {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        RecordingStarter st = new RecordingStarter(s);
        ImeSetup.Started r = ImeSetup.start(false, st, m -> { });
        check(!r.imeSelected, "not selected");
        eq(null, r.imeFailure, "nothing failed");
        eq("[start 1, arm 1]", st.events.toString(), "STAY_ON only");
        r.session.close();
    }

    static void companionStateMessages() throws Exception {
        Msg ready = ImeSetup.companionState(true, null, true);
        eq("companion", ready.t, "type");
        eq("ready", ready.str("state"), "ready");
        eq(1, ready.fields().size(), "no msg when ready");
        Msg kb = ImeSetup.companionState(true, "keyboard switch failed: x", true);
        eq("unavailable", kb.str("state"), "keyboard failed");
        eq("keyboard switch failed: x", kb.str("msg"), "keyboard msg");
        Msg none = ImeSetup.companionState(false, null, true);
        eq("unavailable", none.str("state"), "not linked");
        eq("companion did not connect", none.str("msg"), "not linked msg");
    }

    /** Keyboard off in Lantern's settings: linked and ready, with the reason as msg. */
    static void companionStateKeyboardOff() throws Exception {
        Msg off = ImeSetup.companionState(true, null, false);
        eq("companion", off.t, "type");
        eq("ready", off.str("state"), "linked, keyboard off: ready");
        eq("keyboard off in Lantern settings", off.str("msg"), "msg");
        eq(2, off.fields().size(), "state and msg only");
        Msg none = ImeSetup.companionState(false, null, false);
        eq("unavailable", none.str("state"), "not linked stays unavailable");
        eq("companion did not connect", none.str("msg"), "not linked msg");
    }

    /** While the Lantern keyboard is not selected, ime is dropped, logged once per stretch, never the text. */
    static void relayDropsWhileKeyboardNotSelected() {
        List<String> logs = new ArrayList<>();
        List<Msg> sent = new ArrayList<>();
        boolean[] selected = {false};
        ImeSetup.Relay relay = new ImeSetup.Relay(m -> {
            sent.add(m);
            return true;
        }, () -> selected[0], logs::add, Runnable::run);
        relay.ime(11, "commit", "secret text");
        relay.ime(11, "compose", "more secret");
        eq(0, sent.size(), "nothing reaches the companion");
        eq("[ime dropped: the Lantern keyboard is not selected]", logs.toString(), "one line, no text");
        selected[0] = true;
        relay.ime(11, "commit", "hi");
        eq(1, sent.size(), "delivered once selected");
        selected[0] = false;
        relay.ime(11, "finish", "");
        eq(2, logs.size(), "a new dropping stretch logs once more: " + logs);
    }

    static void relayDropsAndLogsOnceWhileUnlinked() {
        List<String> logs = new ArrayList<>();
        List<Msg> sent = new ArrayList<>();
        boolean[] linked = {false};
        ImeSetup.Relay relay = new ImeSetup.Relay(m -> {
            if (!linked[0]) {
                return false;
            }
            sent.add(m);
            return true;
        }, logs::add, Runnable::run);
        relay.ime(11, "commit", "secret text");
        relay.ime(11, "compose", "more secret");
        eq(1, logs.size(), "one log line for the unlinked stretch: " + logs);
        check(!logs.get(0).contains("secret"), "never logs text: " + logs);
        linked[0] = true;
        relay.ime(12, "commit", "hi");
        eq(1, sent.size(), "sent once linked");
        Msg m = sent.get(0);
        eq("ime", m.t, "t");
        eq("commit", m.fields().get("op"), "op");
        eq("hi", m.fields().get("text"), "text");
        eq(12L, m.fields().get("display"), "the window's display (link-only field)");
        linked[0] = false;
        relay.ime(12, "finish", "");
        relay.ime(12, "finish", "");
        eq(2, logs.size(), "a new unlinked stretch logs once more: " + logs);
    }

    /** The control thread never waits on the companion: a full queue drops, logged once. */
    static void relayNeverBlocksTheCaller() throws Exception {
        List<String> logs = java.util.Collections.synchronizedList(new ArrayList<>());
        java.util.concurrent.CountDownLatch jam = new java.util.concurrent.CountDownLatch(1);
        List<String> sent = java.util.Collections.synchronizedList(new ArrayList<>());
        java.util.concurrent.ExecutorService exec = ImeSetup.Relay.executor(2);
        try {
            ImeSetup.Relay relay = new ImeSetup.Relay(m -> {
                try {
                    jam.await(); // a companion that stopped reading
                } catch (InterruptedException e) {
                    return false;
                }
                sent.add(String.valueOf(m.fields().get("text")));
                return true;
            }, logs::add, exec);
            long t0 = System.nanoTime();
            for (int i = 0; i < 50; i++) {
                relay.ime(11, "commit", "t" + i);
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            check(ms < 500, "50 ime calls returned at once despite the jam: " + ms + " ms");
            eq(1, logs.size(), "overflow logged once: " + logs);
            check(!logs.get(0).contains("t4"), "no text in the log: " + logs);
            jam.countDown();
            exec.shutdown();
            check(exec.awaitTermination(2, java.util.concurrent.TimeUnit.SECONDS), "drained");
            eq("t0", sent.get(0), "in order, first kept");
            check(sent.size() <= 3, "the rest were dropped, not queued without bound: " + sent.size());
        } finally {
            exec.shutdownNow();
        }
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
            {"addsOnce", (Case) ImeSetupTest::addsOnce},
            {"keepsSubtypes", (Case) ImeSetupTest::keepsSubtypes},
            {"nullOrEmptyList", (Case) ImeSetupTest::nullOrEmptyList},
            {"alreadyPresentUnchanged", (Case) ImeSetupTest::alreadyPresentUnchanged},
            {"changesEnableThenSelect", (Case) ImeSetupTest::changesEnableThenSelect},
            {"prevImeFromJournal", (Case) ImeSetupTest::prevImeFromJournal},
            {"relayDropsAndLogsOnceWhileUnlinked", (Case) ImeSetupTest::relayDropsAndLogsOnceWhileUnlinked},
            {"relayNeverBlocksTheCaller", (Case) ImeSetupTest::relayNeverBlocksTheCaller},
            {"imeSwitchFailureFallsBackToStayOn", (Case) ImeSetupTest::imeSwitchFailureFallsBackToStayOn},
            {"noRetryWhenRollbackFailed", (Case) ImeSetupTest::noRetryWhenRollbackFailed},
            {"imeSelectedWhenItWorks", (Case) ImeSetupTest::imeSelectedWhenItWorks},
            {"notLinkedIsStayOnOnly", (Case) ImeSetupTest::notLinkedIsStayOnOnly},
            {"companionStateMessages", (Case) ImeSetupTest::companionStateMessages},
            {"companionStateKeyboardOff", (Case) ImeSetupTest::companionStateKeyboardOff},
            {"relayDropsWhileKeyboardNotSelected", (Case) ImeSetupTest::relayDropsWhileKeyboardNotSelected},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   ImeSetupTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL ImeSetupTest." + c[0] + ": " + t);
            }
        }
        System.out.println("ImeSetupTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
