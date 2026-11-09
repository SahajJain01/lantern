package dev.lantern.companion;

/** State the link shares with the IME. */
final class ImeState {
    /** The keyboard to restore if the link is lost, from the helper's {@code ime_prev}; null until received. */
    static volatile String prevIme;

    /** The running {@link LanternIme}, or null when it is not created (not the selected keyboard). */
    static volatile LanternIme ime;

    /** Told by {@link Link} when the helper links and when the link is lost; polled by {@link LanternIme}. */
    static final ImeWatchdog WATCHDOG = new ImeWatchdog();

    private ImeState() {
    }
}
