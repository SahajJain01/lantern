package dev.lantern.companion;

/**
 * When the Lantern IME gives the keyboard back on its own (pure; {@link LanternIme} polls it). The
 * helper and its Cleanup normally restore the phone's keyboard; this covers the rest (e.g. the
 * helper and Cleanup both gone). After the helper's link is lost, {@link #due} returns the
 * keyboard to restore once {@code graceMs} passed, if the Lantern IME is still the current one,
 * at most once per loss. A link that comes back within the grace (a new session) cancels it.
 * Thread-safe.
 */
final class ImeWatchdog {
    static final long DEFAULT_GRACE_MS = 5000;

    private final long graceMs;
    private boolean down;
    private long lostAt;
    private String prevIme;

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
        lostAt = nowMs;
        this.prevIme = prevIme;
    }

    /**
     * The keyboard to switch to now, or null: only while the link is down, {@code graceMs} after
     * the loss, while {@code weAreCurrent}, with a non-empty {@code prevIme}; once per loss.
     */
    synchronized String due(long nowMs, boolean weAreCurrent) {
        if (!down || nowMs - lostAt < graceMs || !weAreCurrent || prevIme == null || prevIme.isEmpty()) {
            return null;
        }
        String id = prevIme;
        down = false; // fired: not again until the next loss
        prevIme = null;
        return id;
    }
}
