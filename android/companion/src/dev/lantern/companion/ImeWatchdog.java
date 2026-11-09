package dev.lantern.companion;

import java.util.List;
import java.util.function.Supplier;

/**
 * When the Lantern IME gives the keyboard back on its own (pure; {@link LanternIme} polls it). The
 * helper and its Cleanup normally restore the phone's keyboard; this covers the rest (e.g. the
 * helper and Cleanup both gone). After the helper's link is lost, {@link #due} says what to switch
 * to once {@code graceMs} passed, while the Lantern IME is the current one. A link that comes back
 * within the grace (a new session) cancels it.
 *
 * <p>A switch can fail (it throws, or is silently ignored: the original keyboard was uninstalled or
 * disabled), so after each switch it checks again {@link #RETRY_MS} later: if the Lantern IME is
 * still current it tries again, up to {@link #MAX_ATTEMPTS} times; then (or at once if the original
 * is not among the enabled keyboards) it falls back to the first enabled keyboard that is neither
 * Lantern's nor the original. A fallback that does not take, or no candidate, gives up (once, with a
 * reason to log). Thread-safe.
 */
final class ImeWatchdog {
    static final long DEFAULT_GRACE_MS = 5000;
    /** How long after a switch it checks whether the switch took. */
    static final long RETRY_MS = 2000;
    /** Switches to the original keyboard before falling back. */
    static final int MAX_ATTEMPTS = 5;
    /** The Lantern IME: never a fallback. */
    static final String LANTERN_ID = "dev.lantern.companion/.LanternIme";

    /** What to do now: switch to {@link #id} (the original, or a {@link #fallback}), or give up. */
    static final class Action {
        /** The keyboard to switch to; null when giving up. */
        final String id;
        final boolean fallback;
        /** Why it gives up (to log), when {@link #id} is null. */
        final String giveUp;

        private Action(String id, boolean fallback, String giveUp) {
            this.id = id;
            this.fallback = fallback;
            this.giveUp = giveUp;
        }
    }

    private final long graceMs;
    private boolean down;
    /** When to act next (the end of the grace, then each retry). */
    private long nextAt;
    private String prevIme;
    /** Switches to {@link #prevIme} so far for this loss. */
    private int attempts;
    private boolean fallbackTried;

    ImeWatchdog() {
        this(DEFAULT_GRACE_MS);
    }

    ImeWatchdog(long graceMs) {
        this.graceMs = graceMs;
    }

    /** The helper linked: nothing to restore. */
    synchronized void linkUp() {
        down = false;
        prevIme = null;
    }

    /** The link was lost at {@code nowMs}; {@code prevIme} is the last {@code ime_prev} (or null). */
    synchronized void linkLost(long nowMs, String prevIme) {
        down = true;
        nextAt = nowMs + graceMs;
        this.prevIme = prevIme;
        attempts = 0;
        fallbackTried = false;
    }

    /**
     * What to do now, or null. Nothing while the link is up, before the grace (or the next retry)
     * is over, without a non-empty {@code prevIme}, or while another keyboard is current (before
     * the first switch it stays armed; after a switch that means the switch took, which ends it).
     * {@code enabled} (the enabled IME ids) is read only when about to act; if it cannot be read,
     * the original is assumed enabled and no fallback is known.
     */
    synchronized Action due(long nowMs, boolean weAreCurrent, Supplier<List<String>> enabled) {
        if (!down || nowMs < nextAt || prevIme == null || prevIme.isEmpty()) {
            return null;
        }
        if (!weAreCurrent) {
            if (attempts > 0 || fallbackTried) {
                end(); // the last switch took (or someone else picked a keyboard): done
            }
            return null;
        }
        if (fallbackTried) {
            end();
            return new Action(null, false, "fallback keyboard did not take");
        }
        List<String> list;
        try {
            list = enabled.get();
        } catch (RuntimeException e) {
            list = null;
        }
        boolean prevEnabled = list == null || contains(list, prevIme);
        if (prevEnabled && attempts < MAX_ATTEMPTS) {
            attempts++;
            nextAt = nowMs + RETRY_MS;
            return new Action(prevIme, false, null);
        }
        String candidate = null;
        if (list != null) {
            for (String id : list) {
                String c = component(id);
                if (!c.equals(LANTERN_ID) && !c.equals(component(prevIme))) {
                    candidate = id;
                    break;
                }
            }
        }
        if (candidate == null) {
            String why = prevEnabled ? "original keyboard did not take after " + attempts + " attempts"
                : "original keyboard is not enabled";
            end();
            return new Action(null, false, why + "; no other enabled keyboard");
        }
        fallbackTried = true;
        nextAt = nowMs + RETRY_MS;
        return new Action(candidate, true, null);
    }

    private void end() {
        down = false;
        prevIme = null;
    }

    private static boolean contains(List<String> list, String id) {
        String want = component(id);
        for (String e : list) {
            if (e != null && component(e).equals(want)) {
                return true;
            }
        }
        return false;
    }

    /** An IME id without a {@code ;subtype} suffix. */
    private static String component(String id) {
        int semi = id.indexOf(';');
        return semi < 0 ? id : id.substring(0, semi);
    }
}
