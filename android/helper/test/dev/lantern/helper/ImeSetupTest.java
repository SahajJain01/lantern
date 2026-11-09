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
        relay.ime("commit", "secret text");
        relay.ime("compose", "more secret");
        eq(1, logs.size(), "one log line for the unlinked stretch: " + logs);
        check(!logs.get(0).contains("secret"), "never logs text: " + logs);
        linked[0] = true;
        relay.ime("commit", "hi");
        eq(1, sent.size(), "sent once linked");
        Msg m = sent.get(0);
        eq("ime", m.t, "t");
        eq("commit", m.fields().get("op"), "op");
        eq("hi", m.fields().get("text"), "text");
        linked[0] = false;
        relay.ime("finish", "");
        relay.ime("finish", "");
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
                relay.ime("commit", "t" + i);
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
