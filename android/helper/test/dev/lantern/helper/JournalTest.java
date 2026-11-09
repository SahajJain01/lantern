package dev.lantern.helper;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * Host-JVM tests for the multi-entry settings {@link Journal}: its v2 JSON, the Plan 1 flat form,
 * and the restore rules (reverse order, only while the value is still the one Lantern wrote, IME
 * through {@code selectIme}, every entry attempted). Uses {@link SessionTest.FakeSettings}.
 */
public final class JournalTest {
    static Journal.Entry e(String ns, String key, String orig, String set) {
        return new Journal.Entry(ns, key, orig, set);
    }

    static void journalV2RoundTrip() throws Exception {
        Journal j = new Journal(Arrays.asList(
            e("global", "stay_on_while_plugged_in", "15", "7"),
            e("secure", "default_input_method", null, "dev.lantern.companion/.LanternIme"),
            e("system", "odd \"key\"", "a \\ b\n", "x")));
        String json = j.toJson();
        check(json.startsWith("{\"v\":2,\"entries\":[{\"ns\":\"global\""), "v2 shape: " + json);
        Journal back = Journal.parse(json);
        eq(j.entries(), back.entries(), "entries round-trip");
        eq(null, back.find("secure", "default_input_method").orig, "null orig survives");
        eq(null, back.find("global", "missing"), "find of an absent key");
        // Whitespace tolerated, as a hand-edited file might have it.
        Journal spaced = Journal.parse(" { \"v\" : 2 , \"entries\" : [ { \"ns\" : \"global\" , \"key\" : \"k\" ,"
            + " \"orig\" : null , \"set\" : \"7\" } ] } ");
        eq(Arrays.asList(e("global", "k", null, "7")), spaced.entries(), "spaced json");
        eq(new ArrayList<Journal.Entry>(), Journal.parse("{\"v\":2,\"entries\":[]}").entries(), "empty");
    }

    static void parsesLegacyV1AsStayOnEntry() throws Exception {
        Journal j = Journal.parse("{\"ns\":\"global\",\"key\":\"stay_on_while_plugged_in\",\"orig\":\"15\"}");
        eq(Arrays.asList(e("global", "stay_on_while_plugged_in", "15", "7")), j.entries(), "legacy entry");
        Journal absent = Journal.parse("{\"ns\":\"global\",\"key\":\"stay_on_while_plugged_in\",\"orig\":null}");
        eq(null, absent.entries().get(0).orig, "legacy null orig");
    }

    static void rejectsBadJson() throws Exception {
        for (String bad : new String[] {"{not json", "{\"v\":3,\"entries\":[]}", "{\"v\":2}",
                "{\"v\":2,\"entries\":[{\"ns\":\"g\",\"key\":\"k\",\"orig\":\"1\"}]}",
                "{\"v\":2,\"entries\":[{\"ns\":null,\"key\":\"k\",\"orig\":\"1\",\"set\":\"7\"}]}",
                "{\"v\":2,\"entries\":[]} trailing", "{\"ns\":\"g\",\"key\":\"k\"}"}) {
            try {
                Journal.parse(bad);
                throw new AssertionError("accepted " + bad);
            } catch (IllegalArgumentException expected) {
                // rejected
            }
        }
    }

    static void restoreReversesOrder() throws Exception {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        s.values.put("global/a", "7");
        s.values.put("global/b", "9");
        new Journal(Arrays.asList(e("global", "a", "1", "7"), e("global", "b", "2", "9")))
            .restore(s, msg -> { });
        eq(Arrays.asList("put global/b=2", "put global/a=1"), s.writes, "reverse order");
    }

    static void leavesUserChangedValue() throws Exception {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        s.values.put("global/stay_on_while_plugged_in", "3");
        List<String> log = new ArrayList<>();
        new Journal(Arrays.asList(e("global", "stay_on_while_plugged_in", "15", "7"))).restore(s, log::add);
        eq("3", s.get("global", "stay_on_while_plugged_in"), "user value kept");
        eq(0, s.writes.size(), "no writes: " + s.writes);
        eq("[journal: global/stay_on_while_plugged_in changed since the session started (now 3); left as is]",
            log.toString(), "log");
    }

    static void deletesAbsentOriginal() throws Exception {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        s.values.put("global/k", "7");
        new Journal(Arrays.asList(e("global", "k", null, "7"))).restore(s, msg -> { });
        eq(Arrays.asList("delete global/k"), s.writes, "absent original deleted, never put null");
    }

    static void restoresImeWithSelectIme() throws Exception {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings();
        s.values.put("secure/default_input_method", "dev.lantern.companion/.LanternIme");
        new Journal(Arrays.asList(e("secure", "default_input_method", "com.android.inputmethod.latin/.LatinIME",
            "dev.lantern.companion/.LanternIme"))).restore(s, msg -> { });
        eq(Arrays.asList("selectIme com.android.inputmethod.latin/.LatinIME"), s.writes, "selectIme, no put");
        eq("com.android.inputmethod.latin/.LatinIME", s.get("secure", "default_input_method"), "ime back");
    }

    static void restoreAttemptsEveryEntryWhenOneThrows() throws Exception {
        SessionTest.FakeSettings s = new SessionTest.FakeSettings() {
            @Override
            public void put(String ns, String key, String v) {
                if (key.equals("b")) {
                    throw new IllegalStateException("put b failed");
                }
                super.put(ns, key, v);
            }
        };
        s.values.put("global/a", "7");
        s.values.put("global/b", "7");
        s.values.put("global/c", "7");
        Journal j = new Journal(Arrays.asList(e("global", "a", "1", "7"), e("global", "b", "2", "7"),
            e("global", "c", "3", "7")));
        try {
            j.restore(s, msg -> { });
            throw new AssertionError("failure swallowed");
        } catch (IllegalStateException expected) {
            eq("put b failed", expected.getMessage(), "first failure rethrown");
        }
        eq("1", s.get("global", "a"), "a restored after b threw");
        eq("3", s.get("global", "c"), "c restored");
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
            {"journalV2RoundTrip", (Case) JournalTest::journalV2RoundTrip},
            {"parsesLegacyV1AsStayOnEntry", (Case) JournalTest::parsesLegacyV1AsStayOnEntry},
            {"rejectsBadJson", (Case) JournalTest::rejectsBadJson},
            {"restoreReversesOrder", (Case) JournalTest::restoreReversesOrder},
            {"leavesUserChangedValue", (Case) JournalTest::leavesUserChangedValue},
            {"deletesAbsentOriginal", (Case) JournalTest::deletesAbsentOriginal},
            {"restoresImeWithSelectIme", (Case) JournalTest::restoresImeWithSelectIme},
            {"restoreAttemptsEveryEntryWhenOneThrows", (Case) JournalTest::restoreAttemptsEveryEntryWhenOneThrows},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   JournalTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL JournalTest." + c[0] + ": " + t);
            }
        }
        System.out.println("JournalTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
