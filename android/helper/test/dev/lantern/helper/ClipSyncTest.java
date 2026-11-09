package dev.lantern.helper;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Host-JVM tests for {@link ClipSync}: echo, sensitive, size and empty suppression. Pure. */
public final class ClipSyncTest {
    static final class Rig {
        final List<String> toHost = new ArrayList<>();
        final List<String> toPhone = new ArrayList<>();
        final ClipSync sync = new ClipSync(toHost::add, toPhone::add);
    }

    static void sendsPhoneCopies() {
        Rig r = new Rig();
        r.sync.phoneChanged("hello", false);
        eq(Arrays.asList("hello"), r.toHost, "phone copy reaches the host");
    }

    static void noEchoOfHostText() {
        Rig r = new Rig();
        r.sync.fromHost("from-pc");
        eq(Arrays.asList("from-pc"), r.toPhone, "host text written to the phone");
        // Writing the phone clipboard fires the listener with the same text: not sent back.
        r.sync.phoneChanged("from-pc", false);
        eq(new ArrayList<String>(), r.toHost, "host text is not echoed back");
        r.sync.phoneChanged("from-pc", false);
        eq(new ArrayList<String>(), r.toHost, "nor on a repeated listener callback");
        r.sync.phoneChanged("typed-on-phone", false);
        eq(Arrays.asList("typed-on-phone"), r.toHost, "a different phone copy is sent");
        // After a phone copy, copying the host's old text again on the phone is a real change.
        r.sync.phoneChanged("from-pc", false);
        eq(Arrays.asList("typed-on-phone", "from-pc"), r.toHost, "re-copying the older text is sent");
    }

    /** The phone fires its listener more than once per copy (seen on the Xiaomi): sent once. */
    static void repeatedPhoneCallbacksSentOnce() {
        Rig r = new Rig();
        r.sync.phoneChanged("copied", false);
        r.sync.phoneChanged("copied", false);
        eq(Arrays.asList("copied"), r.toHost, "one send per change");
        // After the host's text, the phone's older copy again is a change.
        r.sync.fromHost("pc");
        r.sync.phoneChanged("copied", false);
        eq(Arrays.asList("copied", "copied"), r.toHost, "re-copy after a host change is sent");
    }

    static void skipsSensitive() {
        Rig r = new Rig();
        r.sync.phoneChanged("hunter2", true);
        eq(new ArrayList<String>(), r.toHost, "sensitive clip never leaves the phone");
    }

    static void skipsOversize() {
        Rig r = new Rig();
        char[] max = new char[Proto.MAX_CLIP];
        Arrays.fill(max, 'a');
        r.sync.phoneChanged(new String(max), false);
        eq(1, r.toHost.size(), "exactly MAX_CLIP bytes is sent");
        // MAX_CLIP chars, one of them two UTF-8 bytes: one byte over.
        max[0] = '\u00e9';
        r.sync.phoneChanged(new String(max), false);
        eq(1, r.toHost.size(), "one UTF-8 byte over MAX_CLIP is not sent");
    }

    static void skipsEmpty() {
        Rig r = new Rig();
        r.sync.phoneChanged(null, false);
        r.sync.phoneChanged("", false);
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
            {"repeatedPhoneCallbacksSentOnce", (Case) ClipSyncTest::repeatedPhoneCallbacksSentOnce},
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
