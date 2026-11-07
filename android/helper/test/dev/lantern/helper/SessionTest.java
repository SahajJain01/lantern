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

    /** Stand-in for the Cleanup child: stdout is whatever the test feeds it; records stdin. */
    static final class FakeChild extends Process {
        final java.io.PipedOutputStream stdoutFeed = new java.io.PipedOutputStream();
        final java.io.PipedInputStream stdout;
        final java.io.ByteArrayOutputStream stdin = new java.io.ByteArrayOutputStream();
        volatile boolean destroyed;
        final List<String> command = new ArrayList<>();

        FakeChild() throws java.io.IOException {
            stdout = new java.io.PipedInputStream(stdoutFeed);
        }

        void say(String s) throws java.io.IOException {
            stdoutFeed.write(s.getBytes(java.nio.charset.StandardCharsets.UTF_8));
            stdoutFeed.flush();
        }

        @Override
        public java.io.OutputStream getOutputStream() {
            return stdin;
        }

        @Override
        public java.io.InputStream getInputStream() {
            return stdout;
        }

        @Override
        public java.io.InputStream getErrorStream() {
            return new java.io.ByteArrayInputStream(new byte[0]);
        }

        @Override
        public int waitFor() {
            return 0;
        }

        @Override
        public int exitValue() {
            return 0;
        }

        @Override
        public void destroy() {
            destroyed = true;
            try {
                stdoutFeed.close();
            } catch (java.io.IOException ignored) {
            }
        }

        @Override
        public Process destroyForcibly() {
            destroy();
            return this;
        }

        @Override
        public boolean isAlive() {
            return !destroyed;
        }
    }

    static Cleanup.ProcessGuard fakeGuard(FakeChild child, long timeoutMs) {
        return new Cleanup.ProcessGuard((cmd, cp) -> {
            child.command.addAll(cmd);
            return child;
        }, "/data/local/tmp/x.jar", timeoutMs, msg -> { });
    }

    static void armFailsWithoutReady() throws Exception {
        FakeSettings s = new FakeSettings();
        s.values.put(NS + "/" + KEY, "15");
        FakePower p = new FakePower();
        FakeChild child = new FakeChild(); // never says ready
        long t0 = System.nanoTime();
        Throwable thrown = null;
        try {
            Session.start(s, p, fakeGuard(child, 300));
        } catch (RuntimeException e) {
            thrown = e;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(thrown != null, "start must throw when Cleanup never says ready");
        check(ms >= 250 && ms < 2000, "threw after the timeout, took " + ms + " ms");
        check(child.destroyed, "silent child killed");
        eq("15", s.get(NS, KEY), "value untouched");
        eq("[]", s.writes.toString(), "no settings writes");
        eq("[]", p.calls.toString(), "no power calls");
    }

    static void armFailsWhenChildDiesBeforeReady() throws Exception {
        FakeSettings s = new FakeSettings();
        s.values.put(NS + "/" + KEY, "15");
        FakePower p = new FakePower();
        FakeChild child = new FakeChild();
        child.stdoutFeed.close(); // died during startup: EOF, no ready
        long t0 = System.nanoTime();
        Throwable thrown = null;
        try {
            Session.start(s, p, fakeGuard(child, 3000));
        } catch (RuntimeException e) {
            thrown = e;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(thrown != null, "start must throw when Cleanup exits before ready");
        check(ms < 1000, "EOF fails fast, took " + ms + " ms");
        eq("[]", s.writes.toString(), "no settings writes");
        eq("[]", p.calls.toString(), "no power calls");
    }

    static void armProceedsAfterReady() throws Exception {
        FakeSettings s = new FakeSettings();
        s.values.put(NS + "/" + KEY, "15");
        FakePower p = new FakePower();
        FakeChild child = new FakeChild();
        child.say("ready\n");
        Session session = Session.start(s, p, fakeGuard(child, 3000));
        eq("7", s.get(NS, KEY), "value during session");
        check(!child.destroyed, "ready child left running");
        eq("app_process", child.command.get(0), "launches app_process");
        eq(Cleanup.class.getName(), child.command.get(2), "launches Cleanup");
        eq(session.journal().toJson(), child.command.get(3), "journal passed as arg");
        session.close();
        eq("done\n", child.stdin.toString("UTF-8"), "disarm writes done");
        eq("15", s.get(NS, KEY), "value after close");
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
            {"armFailsWithoutReady", (Case) SessionTest::armFailsWithoutReady},
            {"armFailsWhenChildDiesBeforeReady", (Case) SessionTest::armFailsWhenChildDiesBeforeReady},
            {"armProceedsAfterReady", (Case) SessionTest::armProceedsAfterReady},
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
