package dev.lantern.helper;

/**
 * One helper session: keep the phone awake while plugged in, wake it and turn the built-in panel
 * off. {@link #close()} undoes it in reverse order. The original state is journaled and handed to
 * a {@link Guard} (the out-of-process {@link Cleanup}) before anything is changed, so the phone is
 * restored even if this process is killed.
 */
public final class Session implements AutoCloseable {
    static final String NS = "global";
    static final String KEY = "stay_on_while_plugged_in";
    /** BATTERY_PLUGGED_AC | USB | WIRELESS. */
    static final String STAY_ON = "7";

    /** Crash-safety hook: armed with the journal before any change, disarmed after a clean restore. */
    public interface Guard {
        void arm(String journalJson);

        void disarm();
    }

    private static final Guard NO_GUARD = new Guard() {
        @Override
        public void arm(String journalJson) {
        }

        @Override
        public void disarm() {
        }
    };

    private final SettingsBackend settings;
    private final Power power;
    private final Guard guard;
    private final JournalStore store;
    private final Cleanup.Journal journal;
    private boolean settingWritten;
    private boolean panelOff;
    private boolean closed;

    private Session(SettingsBackend settings, Power power, Guard guard, JournalStore store,
            Cleanup.Journal journal) {
        this.settings = settings;
        this.power = power;
        this.guard = guard;
        this.store = store;
        this.journal = journal;
    }

    public static Session start(SettingsBackend s, Power p) {
        return start(s, p, NO_GUARD);
    }

    public static Session start(SettingsBackend s, Power p, Guard g) {
        return start(s, p, g, JournalStore.none());
    }

    /**
     * The original value comes from the persisted journal if one exists (an earlier session died
     * without restoring: the current value may be its STAY_ON), else from the setting itself. The
     * journal is persisted, then the guard armed, before anything is changed.
     */
    public static Session start(SettingsBackend s, Power p, Guard g, JournalStore store) {
        Cleanup.Journal recovered = store.load();
        Cleanup.Journal journal = recovered != null && NS.equals(recovered.ns) && KEY.equals(recovered.key)
            ? recovered
            : new Cleanup.Journal(NS, KEY, s.get(NS, KEY));
        store.save(journal);
        Session session = new Session(s, p, g, store, journal);
        try {
            g.arm(session.journal.toJson());
        } catch (RuntimeException e) {
            if (journal != recovered) {
                store.clear(); // nothing changed; a recovered journal must survive, though
            }
            throw e;
        }
        try {
            session.settingWritten = true; // set first: a throwing put may still have written
            s.put(NS, KEY, STAY_ON);
            p.wake();
            session.panelOff = true;
            p.setPanel(false);
        } catch (RuntimeException e) {
            try {
                session.close();
            } catch (RuntimeException suppressed) {
                e.addSuppressed(suppressed);
            }
            throw e;
        }
        return session;
    }

    public Cleanup.Journal journal() {
        return journal;
    }

    /**
     * Idempotent. Panel back on, then the original setting (even if the panel call throws an
     * Error), then the persisted journal is removed; the guard is disarmed last.
     */
    @Override
    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        RuntimeException failure = null;
        try {
            if (panelOff) {
                try {
                    power.setPanel(true);
                } catch (RuntimeException e) {
                    failure = e;
                }
            }
        } finally {
            if (settingWritten) {
                try {
                    journal.restore(settings);
                } catch (RuntimeException e) {
                    failure = failure == null ? e : failure;
                }
            }
        }
        if (failure != null) {
            // Leave the guard armed and the journal on disk: Cleanup retries the restore when
            // this process exits, and the next session still knows the original.
            throw failure;
        }
        store.clear();
        guard.disarm();
    }
}
