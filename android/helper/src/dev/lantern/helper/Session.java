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
    private final Cleanup.Journal journal;
    private boolean settingWritten;
    private boolean panelOff;
    private boolean closed;

    private Session(SettingsBackend settings, Power power, Guard guard, Cleanup.Journal journal) {
        this.settings = settings;
        this.power = power;
        this.guard = guard;
        this.journal = journal;
    }

    public static Session start(SettingsBackend s, Power p) {
        return start(s, p, NO_GUARD);
    }

    public static Session start(SettingsBackend s, Power p, Guard g) {
        Session session = new Session(s, p, g, new Cleanup.Journal(NS, KEY, s.get(NS, KEY)));
        g.arm(session.journal.toJson());
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

    /** Idempotent. Panel back on, then the original setting; the guard is disarmed last. */
    @Override
    public synchronized void close() {
        if (closed) {
            return;
        }
        closed = true;
        RuntimeException failure = null;
        if (panelOff) {
            try {
                power.setPanel(true);
            } catch (RuntimeException e) {
                failure = e;
            }
        }
        if (settingWritten) {
            try {
                journal.restore(settings);
            } catch (RuntimeException e) {
                failure = failure == null ? e : failure;
            }
        }
        if (failure != null) {
            // Leave the guard armed: Cleanup retries the restore when this process exits.
            throw failure;
        }
        guard.disarm();
    }
}
