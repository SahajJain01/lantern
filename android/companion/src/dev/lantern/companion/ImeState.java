package dev.lantern.companion;

/** State the link shares with the IME. */
final class ImeState {
    /** The keyboard to restore if the link is lost, from the helper's {@code ime_prev}; null until received. */
    static volatile String prevIme;

    /** The running {@link LanternIme}, or null when it is not created (not the selected keyboard). */
    static volatile LanternIme ime;

    /** Told by {@link Link} when the helper links and when the link is lost; polled by {@link LanternIme}. */
    static final ImeWatchdog WATCHDOG = new ImeWatchdog();

    /**
     * The helper linked: forget the previous session's {@code ime_prev} (the new session sends its
     * own, if it selects the Lantern IME) and cancel any pending watchdog restore. Otherwise a link
     * lost before the new {@code ime_prev} arrives would restore an older session's keyboard.
     */
    static void linkUp() {
        prevIme = null;
        WATCHDOG.linkUp();
    }

    private ImeState() {
    }
}
