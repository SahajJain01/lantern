package dev.lantern.companion;

/**
 * State the link shares with the IME. The keyboard to give back is kept twice: {@link #prevIme}
 * (this link's {@code ime_prev}, in memory) and {@link #store} (the last non-empty
 * {@code ime_prev}, persisted in device-protected storage so it survives a reboot or battery death
 * mid-session, when the Lantern IME stays selected and no helper or Cleanup is left to restore it).
 */
final class ImeState {
    /** This link's {@code ime_prev}; null until received (and again after each {@link #linkUp}). */
    static volatile String prevIme;

    /** The persisted {@code ime_prev}. Starts in memory; the device swaps in {@link PrefsPrevStore}. */
    static volatile PrevStore store = new MemoryStore();

    /** The running {@link LanternIme}, or null when it is not created (not the selected keyboard). */
    static volatile LanternIme ime;

    /** Told by {@link Link} when the helper links and when the link is lost; polled by {@link LanternIme}. */
    static final ImeWatchdog WATCHDOG = new ImeWatchdog();

    /**
     * The helper linked: the in-memory {@code ime_prev} is now stale (the new session sends its
     * own moments later, if it selects the Lantern IME) and any pending watchdog restore is
     * cancelled. The persisted copy is kept: if this link is lost before the new {@code ime_prev}
     * arrives, the previous session's original is still a better restore target than none (it is
     * checked against the enabled keyboards, with a fallback). It is replaced by the next
     * non-empty {@code ime_prev}, or cleared once a watchdog restore took.
     */
    static void linkUp() {
        prevIme = null;
        WATCHDOG.linkUp();
    }

    /** The helper's {@code ime_prev}: kept in memory, and persisted unless empty (unknown). */
    static void imePrev(String id) {
        prevIme = id;
        if (id == null || id.isEmpty()) {
            return; // "" never erases a known original (e.g. the journal held Lantern's own id)
        }
        try {
            store.put(id);
        } catch (RuntimeException e) {
            // Persisting is best effort: the in-memory copy still covers a lost link.
        }
    }

    /** What the watchdog should restore: this link's {@code ime_prev}, else the persisted one, else null. */
    static String restoreTarget() {
        String p = prevIme;
        if (p != null && !p.isEmpty()) {
            return p;
        }
        try {
            String s = store.get();
            return s == null || s.isEmpty() ? null : s;
        } catch (RuntimeException e) {
            return null;
        }
    }

    /** A watchdog restore took: nothing is left to give back. */
    static void restored() {
        prevIme = null;
        try {
            store.clear();
        } catch (RuntimeException e) {
            // best effort; a stale value is only ever used while the Lantern IME is current
        }
    }

    /**
     * Uses {@code s} unless a persistent store is already in place (first caller wins). A value
     * the in-memory default already holds is carried over.
     */
    static synchronized void useStore(PrevStore s) {
        if (store instanceof MemoryStore) {
            String pending = store.get();
            store = s;
            if (pending != null) {
                try {
                    s.put(pending);
                } catch (RuntimeException e) {
                    // best effort, as in imePrev
                }
            }
        }
    }

    /** The default (host tests, and before the device store is set up). */
    static final class MemoryStore implements PrevStore {
        private volatile String value;

        @Override
        public String get() {
            return value;
        }

        @Override
        public void put(String id) {
            value = id;
        }

        @Override
        public void clear() {
            value = null;
        }
    }

    private ImeState() {
    }
}
