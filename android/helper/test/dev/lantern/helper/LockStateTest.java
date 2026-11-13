package dev.lantern.helper;

import java.io.IOException;
import java.util.ArrayList;
import java.util.List;

/** Host-JVM tests for {@link LockState}: parsing {@code dumpsys trust} and the unknown-is-unlocked rule. Pure. */
public final class LockStateTest {
    /**
     * Two users, as on the Motorola (owner plus a work profile with a unified challenge). The work
     * profile's own {@code deviceLocked} line must not be read: only the {@code (current)} user's.
     */
    static String twoUsers(int ownerLocked, int workLocked) {
        return "Trust manager state:\n"
            + " User \"Owner\" (id=0, flags=0x4c13) (current): trustState=UNTRUSTED, trustManaged=0, deviceLocked="
            + ownerLocked + ", isActiveUnlockRunning=0, strongAuthRequired=0x0\n"
            + "   Enabled agents:\n"
            + "   Events:\n"
            + " User \"Work profile\" (id=10, flags=0x1030)(managed profile)\n"
            + "   deviceLocked=" + workLocked + "\n"
            + "\n"
            + "target service=10095/com.google.android.gms/.chimera.PersistentInternalBoundBrokerService@-2147483648\n"
            + "connected=true\n";
    }

    static void parsesCurrentUser() throws Exception {
        eq(Boolean.TRUE, LockState.parseTrust(twoUsers(1, 0)), "owner locked");
        eq(Boolean.FALSE, LockState.parseTrust(twoUsers(0, 1)), "owner unlocked, work line ignored");
        eq(Boolean.TRUE, LockState.parseTrust(twoUsers(1, 1)), "both locked");
        String noCurrent = twoUsers(1, 1).replace(" (current)", "");
        eq(null, LockState.parseTrust(noCurrent), "no (current) line");
        eq(null, LockState.parseTrust(""), "empty dump");
        eq(null, LockState.parseTrust(null), "null dump");
        eq(null, LockState.parseTrust(" User \"Owner\" (id=0) (current): trustManaged=0\n"), "current line without deviceLocked");
        eq(null, LockState.parseTrust(" User \"Owner\" (id=0) (current): deviceLocked=x\n"), "unparsable value");

        eq(true, new LockState(() -> twoUsers(1, 0), s -> { }).locked(), "locked() when locked");
        eq(false, new LockState(() -> twoUsers(0, 1), s -> { }).locked(), "locked() when unlocked");
        eq(false, new LockState(() -> noCurrent, s -> { }).locked(), "unknown state is not locked");
    }

    static void failingSourceIsNotLocked() {
        List<String> logs = new ArrayList<>();
        LockState s = new LockState(() -> {
            throw new IOException("dumpsys timed out");
        }, logs::add);
        eq(false, s.locked(), "a failing dumpsys is not locked");
        eq(false, s.locked(), "again");
        eq(1, logs.size(), "logged once per failing stretch: " + logs);
        LockState r = new LockState(() -> {
            throw new IllegalStateException("boom");
        }, logs::add);
        eq(false, r.locked(), "a throwing source is not locked");
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
            {"parsesCurrentUser", (Case) LockStateTest::parsesCurrentUser},
            {"failingSourceIsNotLocked", (Case) LockStateTest::failingSourceIsNotLocked},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   LockStateTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL LockStateTest." + c[0] + ": " + t);
            }
        }
        System.out.println("LockStateTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
