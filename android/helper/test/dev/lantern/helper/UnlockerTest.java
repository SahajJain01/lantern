package dev.lantern.helper;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Deque;
import java.util.List;

/**
 * Host-JVM tests for {@link Unlocker}: one checked attempt on display 0, the PIN zeroed and the
 * panel back off afterwards, never a PIN in a log. Fakes only.
 */
public final class UnlockerTest {
    static final String PIN = "4826";
    static final String FAILS = "<dumpsys fails>";

    static String trust(int locked) {
        return " User \"Owner\" (id=0, flags=0x4c13) (current): trustManaged=0, deviceLocked=" + locked + "\n";
    }

    /** Everything the Unlocker did, recorded in order. */
    static final class Rig {
        final List<String> keys = new ArrayList<>();
        final List<String> logs = new ArrayList<>();
        final List<Boolean> panel = new ArrayList<>();
        final List<Long> sleeps = new ArrayList<>();
        /** Successive dumpsys trust results; the last one repeats. {@link #FAILS} = the read fails. */
        final Deque<String> trust = new ArrayDeque<>();
        String focus = "3e5a8b1 u0 NotificationShade";
        boolean panelWasOff = true;

        Rig trust(String... dumps) {
            trust.addAll(Arrays.asList(dumps));
            return this;
        }

        Unlocker unlocker() {
            LockState lock = new LockState(() -> {
                String d = trust.size() > 1 ? trust.poll() : trust.peek();
                if (FAILS.equals(d)) {
                    throw new java.io.IOException("dumpsys failed");
                }
                return d;
            }, logs::add);
            Power power = new Power() {
                @Override
                public void wake() {
                    throw new AssertionError("wake() is not the unlock path");
                }

                @Override
                public void setPanel(boolean on) {
                    panel.add(on);
                }
            };
            Unlocker.Keys k = new Unlocker.Keys() {
                @Override
                public void key(int display, int keycode) {
                    keys.add("key d=" + display + " " + keycode);
                }

                @Override
                public void swipeUp(int display) {
                    keys.add("swipe d=" + display);
                }
            };
            return new Unlocker(lock, power, k, () -> FocusProbeTest.spikeDump(focus), sleeps::add, panelWasOff,
                logs::add);
        }
    }

    static List<String> digits(String pin) {
        List<String> out = new ArrayList<>();
        for (char c : pin.toCharArray()) {
            out.add("key d=0 " + (Unlocker.KEYCODE_0 + (c - '0')));
        }
        return out;
    }

    static boolean zeroed(char[] a) {
        for (char c : a) {
            if (c != '\0') {
                return false;
            }
        }
        return true;
    }

    static void typesDigitsThenEnterOnDisplay0() {
        Rig r = new Rig().trust(trust(1), trust(1), trust(0));
        char[] pin = PIN.toCharArray();
        Unlocker.Result res = r.unlocker().unlock(pin);
        eq(true, res.ok, "ok");
        eq(null, res.reason, "no reason on ok");
        List<String> want = new ArrayList<>();
        want.add("key d=0 " + Unlocker.KEYCODE_WAKEUP);
        want.add("swipe d=0");
        want.addAll(digits(PIN));
        want.add("key d=0 " + Unlocker.KEYCODE_ENTER);
        eq(want, r.keys, "every key and the swipe on display 0, in order");
        eq(500L, r.sleeps.get(0), "500 ms after the wake-up");
        eq(1000L, r.sleeps.get(1), "1000 ms after the swipe");
        eq(250L, r.sleeps.get(2), "polls every 250 ms");
        eq(true, zeroed(pin), "pin zeroed");
        eq(Arrays.asList(false), r.panel, "panel back off");
        eq(true, r.logs.contains("unlock: start"), "start logged");
        eq(true, r.logs.stream().anyMatch(l -> l.matches("unlock: ok in \\d+ ms")), "ok logged: " + r.logs);
    }

    static void typesNothingUnlessKeyguardFocused() {
        Rig r = new Rig().trust(trust(1));
        r.focus = "b44 u0 com.google.android.deskclock/com.android.deskclock.screensaver.DreamActivity";
        char[] pin = PIN.toCharArray();
        Unlocker.Result res = r.unlocker().unlock(pin);
        eq(false, res.ok, "not ok");
        eq("lock screen not reached", res.reason, "reason");
        eq(Arrays.asList("key d=0 " + Unlocker.KEYCODE_WAKEUP, "swipe d=0"), r.keys, "no digit, no enter");
        eq(true, r.logs.contains("unlock: lock screen not reached (focus="
            + "com.google.android.deskclock/com.android.deskclock.screensaver.DreamActivity)"), "logged: " + r.logs);
        eq(true, zeroed(pin), "pin zeroed");
        eq(Arrays.asList(false), r.panel, "panel back off");
    }

    static void alreadyUnlockedTypesNothing() {
        Rig r = new Rig().trust(trust(0));
        char[] pin = PIN.toCharArray();
        Unlocker.Result res = r.unlocker().unlock(pin);
        eq(true, res.ok, "ok");
        eq("already unlocked", res.reason, "reason");
        eq(new ArrayList<String>(), r.keys, "nothing sent");
        eq(true, zeroed(pin), "pin zeroed");
    }

    static void lockStateUnknownTypesNothing() {
        Rig r = new Rig().trust(FAILS);
        char[] pin = PIN.toCharArray();
        Unlocker.Result res = r.unlocker().unlock(pin);
        eq(false, res.ok, "not ok");
        eq("lock state unknown", res.reason, "reason");
        eq(new ArrayList<String>(), r.keys, "nothing sent");
        eq(true, zeroed(pin), "pin zeroed");
        // No current-user line is unknown too.
        Rig r2 = new Rig().trust("Trust manager state:\n");
        eq("lock state unknown", r2.unlocker().unlock(PIN.toCharArray()).reason, "no current line");
        eq(new ArrayList<String>(), r2.keys, "nothing sent (no current line)");
    }

    static void wrongPinOneAttemptPanelBackOff() {
        Rig r = new Rig().trust(trust(1));
        char[] pin = PIN.toCharArray();
        Unlocker.Result res = r.unlocker().unlock(pin);
        eq(false, res.ok, "not ok");
        eq("wrong PIN", res.reason, "reason");
        long enters = r.keys.stream().filter(k -> k.equals("key d=0 " + Unlocker.KEYCODE_ENTER)).count();
        eq(1L, enters, "exactly one ENTER");
        eq(digits(PIN).size() + 3, r.keys.size(), "wake, swipe, digits, enter: nothing more");
        long polls = r.sleeps.stream().filter(s -> s == 250L).count();
        eq(12L, polls, "polls for 3 s");
        eq(Arrays.asList(false), r.panel, "setPanel(false)");
        eq(true, zeroed(pin), "pin all zeros");
        eq(true, r.logs.contains("unlock: still locked"), "logged: " + r.logs);
    }

    static void panelLeftAloneWhenItWasOn() {
        Rig r = new Rig().trust(trust(1));
        r.panelWasOff = false;
        r.unlocker().unlock(PIN.toCharArray());
        eq(new ArrayList<Boolean>(), r.panel, "panel untouched");
    }

    static void failingKeysStillZeroAndPanelOff() {
        Rig r = new Rig().trust(trust(1));
        LockState lock = new LockState(() -> trust(1), r.logs::add);
        List<Boolean> panel = new ArrayList<>();
        Power power = new Power() {
            @Override
            public void wake() {
            }

            @Override
            public void setPanel(boolean on) {
                panel.add(on);
            }
        };
        Unlocker.Keys keys = new Unlocker.Keys() {
            @Override
            public void key(int display, int keycode) {
                throw new IllegalStateException("inject failed");
            }

            @Override
            public void swipeUp(int display) {
            }
        };
        char[] pin = PIN.toCharArray();
        try {
            new Unlocker(lock, power, keys, () -> "", ms -> { }, true, r.logs::add).unlock(pin);
            throw new AssertionError("expected the inject failure to propagate");
        } catch (IllegalStateException expected) {
            // Main turns it into unlock_result{ok:false}
        }
        eq(true, zeroed(pin), "pin zeroed on failure");
        eq(Arrays.asList(false), panel, "panel back off on failure");
    }

    static void unlockNeverLogsPin() {
        List<String> all = new ArrayList<>();
        Rig ok = new Rig().trust(trust(1), trust(0));
        ok.unlocker().unlock(PIN.toCharArray());
        all.addAll(ok.logs);
        Rig wrong = new Rig().trust(trust(1));
        wrong.unlocker().unlock(PIN.toCharArray());
        all.addAll(wrong.logs);
        Rig away = new Rig().trust(trust(1));
        away.focus = "b44 u0 DreamActivity";
        away.unlocker().unlock(PIN.toCharArray());
        all.addAll(away.logs);
        for (String l : all) {
            if (l.contains(PIN)) {
                throw new AssertionError("PIN in a log line: " + l);
            }
        }
        eq(true, all.size() >= 6, "the attempts logged something: " + all);
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
            {"typesDigitsThenEnterOnDisplay0", (Case) UnlockerTest::typesDigitsThenEnterOnDisplay0},
            {"typesNothingUnlessKeyguardFocused", (Case) UnlockerTest::typesNothingUnlessKeyguardFocused},
            {"alreadyUnlockedTypesNothing", (Case) UnlockerTest::alreadyUnlockedTypesNothing},
            {"lockStateUnknownTypesNothing", (Case) UnlockerTest::lockStateUnknownTypesNothing},
            {"wrongPinOneAttemptPanelBackOff", (Case) UnlockerTest::wrongPinOneAttemptPanelBackOff},
            {"panelLeftAloneWhenItWasOn", (Case) UnlockerTest::panelLeftAloneWhenItWasOn},
            {"failingKeysStillZeroAndPanelOff", (Case) UnlockerTest::failingKeysStillZeroAndPanelOff},
            {"unlockNeverLogsPin", (Case) UnlockerTest::unlockNeverLogsPin},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   UnlockerTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL UnlockerTest." + c[0] + ": " + t);
            }
        }
        System.out.println("UnlockerTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
