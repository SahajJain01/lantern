package dev.lantern.companion;

/** State the link hands to the IME (Task 7's watchdog). */
final class ImeState {
    /** The keyboard to restore if the link is lost, from the helper's {@code ime_prev}; null until received. */
    static volatile String prevIme;

    private ImeState() {
    }
}
