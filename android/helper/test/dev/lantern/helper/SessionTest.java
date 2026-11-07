package dev.lantern.helper;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * Host-JVM tests for {@link Session} and {@link Cleanup}'s journal logic. Uses fake settings and
 * power backends; Android classes must never be loaded here.
 */
public final class SessionTest {
    private static final String NS = "global";
    private static final String KEY = "stay_on_while_plugged_in";

    /** In-memory settings; absent keys are simply missing from the map. Records every write. */
    static final class FakeSettings implements SettingsBackend {
        final Map<String, String> values = new HashMap<>();
        final List<String> writes = new ArrayList<>();

        @Override
        public String get(String ns, String key) {
            return values.get(ns + "/" + key);
        }

        @Override
        public void put(String ns, String key, String v) {
            writes.add("put " + ns + "/" + key + "=" + v);
            values.put(ns + "/" + key, v);
        }

        @Override
        public void delete(String ns, String key) {
            writes.add("delete " + ns + "/" + key);
            values.remove(ns + "/" + key);
        }
    }

    static final class FakePower implements Power {
        final List<String> calls = new ArrayList<>();

        @Override
        public void wake() {
            calls.add("wake");
        }

        @Override
        public void setPanel(boolean on) {
            calls.add("panel(" + on + ")");
        }
    }

    static void restoresExistingValue() throws Exception {
        FakeSettings s = new FakeSettings();
        s.values.put(NS + "/" + KEY, "15");
        FakePower p = new FakePower();
        Session session = Session.start(s, p);
        eq("7", s.get(NS, KEY), "value during session");
        session.close();
        eq("15", s.get(NS, KEY), "value after close");
    }

    static void deletesOriginallyAbsentKey() throws Exception {
        FakeSettings s = new FakeSettings();
        FakePower p = new FakePower();
        Session session = Session.start(s, p);
        eq("7", s.get(NS, KEY), "value during session");
        session.close();
        check(!s.values.containsKey(NS + "/" + KEY), "key absent after close, got " + s.values);
        for (String w : s.writes) {
            check(!w.endsWith("=null"), "wrote literal null: " + w);
        }
        check(s.writes.contains("delete " + NS + "/" + KEY), "deleted, writes=" + s.writes);
    }

    static void closeIsIdempotent() throws Exception {
        FakeSettings s = new FakeSettings();
        s.values.put(NS + "/" + KEY, "15");
        FakePower p = new FakePower();
        Session session = Session.start(s, p);
        session.close();
        int writes = s.writes.size();
        int calls = p.calls.size();
        s.values.put(NS + "/" + KEY, "3"); // changed by someone else after close
        session.close();
        eq(writes, s.writes.size(), "no settings writes on second close");
        eq(calls, p.calls.size(), "no power calls on second close");
        eq("3", s.get(NS, KEY), "second close leaves value alone");
    }

    static void panelTurnedOnAtClose() throws Exception {
        FakeSettings s = new FakeSettings();
        FakePower p = new FakePower();
        Session session = Session.start(s, p);
        eq("[wake, panel(false)]", p.calls.toString(), "power calls after start");
        session.close();
        eq("[wake, panel(false), panel(true)]", p.calls.toString(), "power calls after close");
    }

    static void journalJsonRoundTrip() throws Exception {
        for (String orig : new String[] {"15", null, "a \"quoted\" \\ value"}) {
            FakeSettings s = new FakeSettings();
            if (orig != null) {
                s.values.put(NS + "/" + KEY, orig);
            }
            List<String> armed = new ArrayList<>();
            Session session = Session.start(s, new FakePower(), new Session.Guard() {
                @Override
                public void arm(String journalJson) {
                    armed.add(journalJson);
                }

                @Override
                public void disarm() {
                    armed.add("disarm");
                }
            });
            eq(1, armed.size(), "guard armed once before close");
            Cleanup.Journal j = Cleanup.Journal.parse(armed.get(0));
            eq(NS, j.ns, "ns");
            eq(KEY, j.key, "key");
            eq(orig, j.orig, "orig");
            eq(session.journal().toJson(), armed.get(0), "journal json");

            // Cleanup applying that journal (parent died) restores the original and the panel.
            FakePower cp = new FakePower();
            Cleanup.apply(j, s, cp, msg -> { });
            eq(orig, s.get(NS, KEY), "cleanup restored orig");
            eq("[panel(true)]", cp.calls.toString(), "cleanup panel");
            session.close();
            eq("disarm", armed.get(armed.size() - 1), "guard disarmed on close");
        }
    }

    static void guardArmedBeforeAnyChange() throws Exception {
        FakeSettings s = new FakeSettings();
        s.values.put(NS + "/" + KEY, "15");
        FakePower p = new FakePower();
        List<String> seen = new ArrayList<>();
        Session.start(s, p, new Session.Guard() {
            @Override
            public void arm(String journalJson) {
                seen.add(s.get(NS, KEY) + " " + p.calls);
            }

            @Override
            public void disarm() {
            }
        }).close();
        eq("[15 []]", seen.toString(), "state when guard armed");
    }

    static void cleanupSkipsAfterDone() throws Exception {
        eq(true, Cleanup.shouldRestore(new java.io.ByteArrayInputStream(new byte[0])), "EOF restores");
        eq(true, Cleanup.shouldRestore(new java.io.ByteArrayInputStream("noise\n".getBytes())), "garbage restores");
        eq(false, Cleanup.shouldRestore(new java.io.ByteArrayInputStream("done\n".getBytes())), "done skips");
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
            {"restoresExistingValue", (Case) SessionTest::restoresExistingValue},
            {"deletesOriginallyAbsentKey", (Case) SessionTest::deletesOriginallyAbsentKey},
            {"closeIsIdempotent", (Case) SessionTest::closeIsIdempotent},
            {"panelTurnedOnAtClose", (Case) SessionTest::panelTurnedOnAtClose},
            {"journalJsonRoundTrip", (Case) SessionTest::journalJsonRoundTrip},
            {"guardArmedBeforeAnyChange", (Case) SessionTest::guardArmedBeforeAnyChange},
            {"cleanupSkipsAfterDone", (Case) SessionTest::cleanupSkipsAfterDone},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   SessionTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL SessionTest." + c[0] + ": " + t);
            }
        }
        System.out.println("SessionTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
