package dev.lantern.helper;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;

/**
 * Host-JVM tests for the session journal persisted on the phone ({@link JournalStore}): it is
 * written before any change, survives a session that never restores (helper and Cleanup killed,
 * reboot), is used instead of the current value by the next session, and is removed only after a
 * clean restore. Uses a temp dir and the fakes from {@link SessionTest}.
 */
public final class JournalStoreTest {
    private static final String NS = "global";
    private static final String KEY = "stay_on_while_plugged_in";

    static File tempDir() throws Exception {
        File d = Files.createTempDirectory("lantern-journal-test").toFile();
        d.deleteOnExit();
        return d;
    }

    static JournalStore store(File dir, List<String> log) {
        return new JournalStore(new File(dir, "lantern-session.json"), log::add);
    }

    static SessionTest.FakeSettings settings(String v) {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        if (v != null) {
            s.values.put(NS + "/" + KEY, v);
        }
        return s;
    }

    static final Session.Guard NO_GUARD = new Session.Guard() {
        @Override
        public void arm(String journalJson) {
        }

        @Override
        public void disarm() {
        }
    };

    static void saveLoadClearRoundTrip() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        eq(null, st.load(), "absent file loads null");
        for (String orig : new String[] {"15", null, "a \"q\" \\ v"}) {
            st.save(new Cleanup.Journal(NS, KEY, orig));
            Cleanup.Journal j = st.load();
            eq(orig, j.orig, "orig");
            eq(KEY, j.key, "key");
            eq("[lantern-session.json]", java.util.Arrays.toString(dir.list()), "no tmp file left");
        }
        st.clear();
        eq(null, st.load(), "cleared");
        eq(0, dir.list().length, "file removed");
        st.clear(); // idempotent
    }

    static void journalPersistedBeforeAnyChange() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        SessionTest.FakeSettings s = settings("15");
        SessionTest.FakePower p = new SessionTest.FakePower();
        List<String> seen = new ArrayList<>();
        Session session = Session.start(s, p, new Session.Guard() {
            @Override
            public void arm(String journalJson) {
                Cleanup.Journal j = st.load();
                seen.add((j == null ? "none" : j.orig) + " " + s.get(NS, KEY) + " " + p.calls);
            }

            @Override
            public void disarm() {
            }
        }, st);
        eq("[15 15 []]", seen.toString(), "journal on disk before arm and before any change");
        eq("15", st.load().orig, "journal kept during the session");
        session.close();
        eq("15", s.get(NS, KEY), "restored");
        eq(null, st.load(), "journal removed after a clean restore");
    }

    /**
     * I3: the previous session died without restoring (helper and Cleanup killed, or a reboot):
     * the setting still reads 7, but the next session must restore the TRUE original from the
     * persisted journal, not the 7 it finds.
     */
    static void nextSessionUsesPersistedOriginal() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        SessionTest.FakeSettings s = settings("15");
        Session.start(s, new SessionTest.FakePower(), NO_GUARD, st); // never closed: killed
        eq("7", s.get(NS, KEY), "phone left stay-awake");

        List<String> armed = new ArrayList<>();
        Session next = Session.start(s, new SessionTest.FakePower(), new Session.Guard() {
            @Override
            public void arm(String journalJson) {
                armed.add(journalJson);
            }

            @Override
            public void disarm() {
            }
        }, st);
        eq("15", next.journal().orig, "recovered original");
        eq("15", Cleanup.Journal.parse(armed.get(0)).orig, "Cleanup armed with the recovered original");
        next.close();
        eq("15", s.get(NS, KEY), "true original restored");
        eq(null, st.load(), "journal removed");
    }

    static void recoveredAbsentKeyIsDeleted() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        SessionTest.FakeSettings s = settings(null);
        Session.start(s, new SessionTest.FakePower(), NO_GUARD, st);
        Session next = Session.start(s, new SessionTest.FakePower(), NO_GUARD, st);
        next.close();
        check(!s.values.containsKey(NS + "/" + KEY), "key absent again: " + s.values);
    }

    static void corruptJournalFallsBackToCurrentValue() throws Exception {
        File dir = tempDir();
        List<String> log = new ArrayList<>();
        JournalStore st = store(dir, log);
        Files.write(new File(dir, "lantern-session.json").toPath(), "{not json".getBytes(StandardCharsets.UTF_8));
        SessionTest.FakeSettings s = settings("15");
        Session session = Session.start(s, new SessionTest.FakePower(), NO_GUARD, st);
        eq("15", session.journal().orig, "current value used");
        check(log.toString().contains("unreadable"), "logged: " + log);
        session.close();
        eq("15", s.get(NS, KEY), "restored");
    }

    static void journalForAnotherKeyIsIgnored() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        st.save(new Cleanup.Journal(NS, "other_key", "1"));
        Session session = Session.start(settings("15"), new SessionTest.FakePower(), NO_GUARD, st);
        eq("15", session.journal().orig, "only a journal of our key is trusted");
        session.close();
    }

    static void cleanupApplyClearsJournalAfterRestore() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        SessionTest.FakeSettings s = settings("15");
        Session session = Session.start(s, new SessionTest.FakePower(), NO_GUARD, st);
        Cleanup.apply(session.journal(), s, new SessionTest.FakePower(), st, msg -> { });
        eq("15", s.get(NS, KEY), "restored");
        eq(null, st.load(), "journal removed by Cleanup");
    }

    static void cleanupKeepsJournalWhenRestoreFails() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        st.save(new Cleanup.Journal(NS, KEY, "15"));
        SettingsBackend broken = new SettingsBackend() {
            @Override
            public String get(String ns, String key) {
                return "7";
            }

            @Override
            public void put(String ns, String key, String v) {
                throw new IllegalStateException("settings put failed");
            }

            @Override
            public void delete(String ns, String key) {
                throw new IllegalStateException("settings delete failed");
            }
        };
        try {
            Cleanup.apply(st.load(), broken, new SessionTest.FakePower(), st, msg -> { });
            throw new AssertionError("restore failure swallowed");
        } catch (IllegalStateException expected) {
            // propagated
        }
        eq("15", st.load().orig, "journal kept for the next session");
    }

    /** Promoted minor: an Error (not a RuntimeException) from setPanel must not skip the restore. */
    static void cleanupRestoresSettingEvenIfPanelThrowsError() throws Exception {
        SessionTest.FakeSettings s = settings("7");
        Power p = new Power() {
            @Override
            public void wake() {
            }

            @Override
            public void setPanel(boolean on) {
                throw new LinkageError("no such method");
            }
        };
        try {
            Cleanup.apply(new Cleanup.Journal(NS, KEY, "15"), s, p, msg -> { });
        } catch (LinkageError expected) {
            // may propagate; the restore must have happened first
        }
        eq("15", s.get(NS, KEY), "setting restored despite the Error");
    }

    static void sessionCloseRestoresSettingEvenIfPanelThrowsError() throws Exception {
        File dir = tempDir();
        JournalStore st = store(dir, new ArrayList<>());
        SessionTest.FakeSettings s = settings("15");
        boolean[] fail = {false};
        Power p = new Power() {
            @Override
            public void wake() {
            }

            @Override
            public void setPanel(boolean on) {
                if (fail[0]) {
                    throw new LinkageError("no such method");
                }
            }
        };
        Session session = Session.start(s, p, NO_GUARD, st);
        fail[0] = true;
        try {
            session.close();
        } catch (LinkageError expected) {
            // may propagate
        }
        eq("15", s.get(NS, KEY), "setting restored despite the Error");
    }

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
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
            {"saveLoadClearRoundTrip", (Case) JournalStoreTest::saveLoadClearRoundTrip},
            {"journalPersistedBeforeAnyChange", (Case) JournalStoreTest::journalPersistedBeforeAnyChange},
            {"nextSessionUsesPersistedOriginal", (Case) JournalStoreTest::nextSessionUsesPersistedOriginal},
            {"recoveredAbsentKeyIsDeleted", (Case) JournalStoreTest::recoveredAbsentKeyIsDeleted},
            {"corruptJournalFallsBackToCurrentValue", (Case) JournalStoreTest::corruptJournalFallsBackToCurrentValue},
            {"journalForAnotherKeyIsIgnored", (Case) JournalStoreTest::journalForAnotherKeyIsIgnored},
            {"cleanupApplyClearsJournalAfterRestore", (Case) JournalStoreTest::cleanupApplyClearsJournalAfterRestore},
            {"cleanupKeepsJournalWhenRestoreFails", (Case) JournalStoreTest::cleanupKeepsJournalWhenRestoreFails},
            {"cleanupRestoresSettingEvenIfPanelThrowsError",
                (Case) JournalStoreTest::cleanupRestoresSettingEvenIfPanelThrowsError},
            {"sessionCloseRestoresSettingEvenIfPanelThrowsError",
                (Case) JournalStoreTest::sessionCloseRestoresSettingEvenIfPanelThrowsError},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   JournalStoreTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL JournalStoreTest." + c[0] + ": " + t);
            }
        }
        System.out.println("JournalStoreTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
