package dev.lantern.companion;

/**
 * Where the companion keeps the last {@code ime_prev} so it survives a reboot (pure interface; on
 * the device {@link PrefsPrevStore}, in device-protected storage). Implementations may throw; the
 * callers in {@link ImeState} treat that as "unknown".
 */
interface PrevStore {
    /** The persisted keyboard id, or null. */
    String get();

    void put(String id);

    void clear();
}
