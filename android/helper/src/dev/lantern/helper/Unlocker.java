package dev.lantern.helper;

import java.io.IOException;
import java.util.Arrays;
import java.util.function.Consumer;

/**
 * One unlock attempt on the phone's PIN keyguard, from the spike (design doc, "Spike result"):
 * wake display 0, swipe up (ends a charging screensaver, opens the PIN pad), and only if display
 * 0's focus is then the keyguard, type the digits and ENTER there. Every key and the swipe target
 * display 0 explicitly: an untargeted key goes to the top-focused display, usually a Lantern
 * window. One attempt, no retry. The PIN is never logged, and its array is zeroed when done.
 */
final class Unlocker {
    // android.view.KeyEvent.KEYCODE_*
    static final int KEYCODE_0 = 7;
    static final int KEYCODE_ENTER = 66;
    static final int KEYCODE_WAKEUP = 224;

    static final int DISPLAY = 0;
    static final long AFTER_WAKE_MS = 500;
    static final long AFTER_SWIPE_MS = 1000;
    static final long POLL_MS = 250;
    static final long POLL_FOR_MS = 3000;

    /** Input on one display. */
    interface Keys {
        /** A key down and up. */
        void key(int display, int keycode);

        /** A swipe from (w/2, 0.81h) to (w/2, 0.22h) over 200 ms; w and h the display's real size. */
        void swipeUp(int display);
    }

    interface Sleeper {
        void sleep(long ms);
    }

    static final class Result {
        final boolean ok;
        /** Null on a plain success. */
        final String reason;

        Result(boolean ok, String reason) {
            this.ok = ok;
            this.reason = reason;
        }
    }

    private final LockState lock;
    private final Power power;
    private final Keys keys;
    private final FocusProbe.Source focus;
    private final Sleeper sleep;
    private final boolean panelWasOff;
    private final Consumer<String> log;

    /** @param panelWasOff the session had turned the panel off: it goes back off afterwards */
    Unlocker(LockState lock, Power power, Keys keys, FocusProbe.Source focus, Sleeper sleep, boolean panelWasOff,
            Consumer<String> log) {
        this.lock = lock;
        this.power = power;
        this.keys = keys;
        this.focus = focus;
        this.sleep = sleep;
        this.panelWasOff = panelWasOff;
        this.log = log;
    }

    /**
     * Unlocks with {@code pin} (ASCII digits, validated by the Router). Zeroes {@code pin} and, if
     * the panel was off, turns it back off, whatever happens. A failing key injection propagates.
     */
    Result unlock(char[] pin) {
        boolean touched = false;
        try {
            Boolean state = lock.read();
            if (Boolean.FALSE.equals(state)) {
                return new Result(true, "already unlocked");
            }
            if (state == null) {
                return new Result(false, "lock state unknown");
            }
            log.accept("unlock: start");
            long t0 = System.nanoTime();
            touched = true;
            keys.key(DISPLAY, KEYCODE_WAKEUP);
            sleep.sleep(AFTER_WAKE_MS);
            keys.swipeUp(DISPLAY);
            sleep.sleep(AFTER_SWIPE_MS);
            String title = focusTitle();
            if (!FocusProbe.isKeyguard(title)) {
                log.accept("unlock: lock screen not reached (focus=" + title + ")");
                return new Result(false, "lock screen not reached");
            }
            // Re-read right before typing: unlocked meanwhile (by hand) means no digit at all.
            Boolean now = lock.read();
            if (Boolean.FALSE.equals(now)) {
                log.accept("unlock: unlocked before typing");
                return new Result(true, "already unlocked");
            }
            if (now == null) {
                log.accept("unlock: lock state unknown before typing");
                return new Result(false, "lock state unknown");
            }
            for (char c : pin) {
                keys.key(DISPLAY, KEYCODE_0 + (c - '0'));
            }
            keys.key(DISPLAY, KEYCODE_ENTER);
            // Only a successful read of locked makes it a wrong PIN; if no read after ENTER
            // succeeded, saying "wrong PIN" would invite guesses towards a lockout.
            Boolean last = null;
            for (long waited = 0; waited < POLL_FOR_MS; waited += POLL_MS) {
                sleep.sleep(POLL_MS);
                Boolean s = lock.read();
                if (Boolean.FALSE.equals(s)) {
                    log.accept("unlock: ok in " + (System.nanoTime() - t0) / 1_000_000 + " ms");
                    return new Result(true, null);
                }
                if (s != null) {
                    last = s;
                }
            }
            if (last == null) {
                log.accept("unlock: not confirmed");
                return new Result(false, "unlock not confirmed");
            }
            log.accept("unlock: still locked");
            return new Result(false, "wrong PIN");
        } finally {
            Arrays.fill(pin, '\0');
            if (touched && panelWasOff) {
                try {
                    power.setPanel(false);
                } catch (RuntimeException e) {
                    log.accept("unlock: panel off failed: " + e);
                }
            }
        }
    }

    /** Display 0's focus title; null if the dump fails or names none (never the keyguard then). */
    private String focusTitle() {
        try {
            return FocusProbe.focusOfDisplay(focus.dump(), DISPLAY);
        } catch (IOException | RuntimeException e) {
            log.accept("unlock: focus probe failed: " + e);
            return null;
        }
    }
}
