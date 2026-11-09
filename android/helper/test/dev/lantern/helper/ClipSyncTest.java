package dev.lantern.helper;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Host-JVM tests for {@link ClipSync}: echo, double-fire, sensitive, size and empty handling. Pure. */
public final class ClipSyncTest {
    static final class Rig {
        final List<String> toHost = new ArrayList<>();
        final List<String> toPhone = new ArrayList<>();
        final ClipSync sync = new ClipSync(toHost::add, toPhone::add);
    }

    static void sendsPhoneCopies() {
        Rig r = new Rig();
        r.sync.phoneChanged("hello", false, 1);
        eq(Arrays.asList("hello"), r.toHost, "phone copy reaches the host");
    }

    static void noEchoOfHostText() {
        Rig r = new Rig();
        r.sync.fromHost("from-pc");
        eq(Arrays.asList("from-pc"), r.toPhone, "host text written to the phone");
        // Writing the phone clipboard fires the listener with the same text: not sent back.
        r.sync.phoneChanged("from-pc", false, 10);
        eq(new ArrayList<String>(), r.toHost, "host text is not echoed back");
        r.sync.phoneChanged("from-pc", false, 10);
        eq(new ArrayList<String>(), r.toHost, "nor on a repeated listener callback");
        r.sync.phoneChanged("typed-on-phone", false, 11);
        eq(Arrays.asList("typed-on-phone"), r.toHost, "a different phone copy is sent");
        // After a phone copy, copying the host's old text again on the phone is a real change.
        r.sync.phoneChanged("from-pc", false, 12);
        eq(Arrays.asList("typed-on-phone", "from-pc"), r.toHost, "re-copying the older text is sent");
    }

    /** The phone fires its listener twice per copy (seen on the Xiaomi): same clip stamp, sent once. */
    static void doubleFireSentOnce() {
        Rig r = new Rig();
        r.sync.phoneChanged("copied", false, 5);
        r.sync.phoneChanged("copied", false, 5);
        eq(Arrays.asList("copied"), r.toHost, "one send per clip");
    }

    /** The phone's own sent text is not remembered: copying it again (new clip) is sent. */
    static void sameTextNewClipAfterChangeIsSent() {
        Rig r = new Rig();
        r.sync.phoneChanged("x", false, 1);
        r.sync.phoneChanged("y", false, 2);
        r.sync.phoneChanged("x", false, 3);
        eq(Arrays.asList("x", "y", "x"), r.toHost, "every new clip is sent");
        // A dropped send (lanternd or the PC clipboard unavailable) cannot poison a re-copy.
        r.sync.phoneChanged("x", false, 4);
        eq(Arrays.asList("x", "y", "x", "x"), r.toHost, "a new clip of the same text is sent");
    }

    /** PC X -> phone, then a sensitive phone clip, then the PC's X again: the phone gets X. */
    static void unsyncablePhoneChangeResetsEcho() {
        String[][] cases = {{"sensitive"}, {"empty"}, {"null"}, {"oversize"}};
        char[] big = new char[Proto.MAX_CLIP + 1];
        Arrays.fill(big, 'a');
        for (String[] c : cases) {
            Rig r = new Rig();
            r.sync.fromHost("x");
            r.sync.phoneChanged("x", false, 1); // echo of the write
            switch (c[0]) {
                case "sensitive": r.sync.phoneChanged("hunter2", true, 2); break;
                case "empty": r.sync.phoneChanged("", false, 2); break;
                case "null": r.sync.phoneChanged(null, false, 2); break;
                default: r.sync.phoneChanged(new String(big), false, 2); break;
            }
            eq(new ArrayList<String>(), r.toHost, c[0] + ": nothing sent yet");
            // The user copies X on the phone again: the PC must get it (it may hold something else).
            r.sync.phoneChanged("x", false, 3);
            eq(Arrays.asList("x"), r.toHost, c[0] + ": re-copy after an unsyncable change is sent");
        }
    }

    static void skipsSensitive() {
        Rig r = new Rig();
        r.sync.phoneChanged("hunter2", true, 1);
        eq(new ArrayList<String>(), r.toHost, "sensitive clip never leaves the phone");
    }

    static void skipsOversize() {
        Rig r = new Rig();
        char[] max = new char[Proto.MAX_CLIP];
        Arrays.fill(max, 'a');
        r.sync.phoneChanged(new String(max), false, 1);
        eq(1, r.toHost.size(), "exactly MAX_CLIP bytes is sent");
        // MAX_CLIP chars, one of them two UTF-8 bytes: one byte over.
        max[0] = '\u00e9';
        r.sync.phoneChanged(new String(max), false, 2);
        eq(1, r.toHost.size(), "one UTF-8 byte over MAX_CLIP is not sent");
    }

    static void skipsEmpty() {
        Rig r = new Rig();
        r.sync.phoneChanged(null, false, 1);
        r.sync.phoneChanged("", false, 2);
        eq(new ArrayList<String>(), r.toHost, "null/empty clips are not sent");
    }

    static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"sendsPhoneCopies", (Case) ClipSyncTest::sendsPhoneCopies},
            {"noEchoOfHostText", (Case) ClipSyncTest::noEchoOfHostText},
            {"doubleFireSentOnce", (Case) ClipSyncTest::doubleFireSentOnce},
            {"sameTextNewClipAfterChangeIsSent", (Case) ClipSyncTest::sameTextNewClipAfterChangeIsSent},
            {"unsyncablePhoneChangeResetsEcho", (Case) ClipSyncTest::unsyncablePhoneChangeResetsEcho},
            {"skipsSensitive", (Case) ClipSyncTest::skipsSensitive},
            {"skipsOversize", (Case) ClipSyncTest::skipsOversize},
            {"skipsEmpty", (Case) ClipSyncTest::skipsEmpty},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   ClipSyncTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL ClipSyncTest." + c[0] + ": " + t);
            }
        }
        System.out.println("ClipSyncTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
