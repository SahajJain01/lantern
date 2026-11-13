package dev.lantern.helper;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Objects;
import java.util.function.Consumer;

/**
 * One helper session: apply a list of setting {@link Change}s (by default only {@link #STAY_ON}),
 * wake the phone and turn the built-in panel off. {@link #close()} undoes it in reverse order. Every
 * change is journaled ({@link Journal}), persisted and handed to a {@link Guard} (the
 * out-of-process {@link Cleanup}) before anything is changed, so the phone is restored even if this
 * process is killed.
 */
public final class Session implements AutoCloseable {
    static final String NS = "global";
    static final String KEY = "stay_on_while_plugged_in";
    /** BATTERY_PLUGGED_AC | USB | WIRELESS. */
    static final String STAY_ON_VALUE = Journal.LEGACY_SET;
    /** Keep the phone awake on every power source while plugged in. */
    public static final Change STAY_ON = Change.setting(NS, KEY, current -> STAY_ON_VALUE);

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
    private final Journal journal;
    private final Consumer<String> log;
    private boolean settingWritten;
    private boolean panelOff;
    private boolean closed;

    private Session(SettingsBackend settings, Power power, Guard guard, JournalStore store, Journal journal) {
        this.settings = settings;
        this.power = power;
        this.guard = guard;
        this.store = store;
        this.journal = journal;
        this.log = store.log();
    }

    public static Session start(SettingsBackend s, Power p) {
        return start(s, p, NO_GUARD);
    }

    public static Session start(SettingsBackend s, Power p, Guard g) {
        return start(s, p, g, JournalStore.none());
    }

    public static Session start(SettingsBackend s, Power p, Guard g, JournalStore store) {
        return start(s, p, g, store, Collections.singletonList(STAY_ON));
    }

    /**
     * Journals every change, persists the journal, arms the guard, and only then applies the
     * changes in list order, wakes the phone and turns the panel off.
     *
     * <p>An earlier session that died without restoring may have left its journal on disk. Its
     * original for a setting is trusted only while the setting still holds the value that session
     * wrote; if anyone changed it since, the current value is the original. Recovered entries for
     * settings this session does not change, still holding their written value, are carried over
     * (first, so they are restored last) and restored at the end too.
     */
    public static Session start(SettingsBackend s, Power p, Guard g, JournalStore store, List<Change> changes) {
        Journal recovered = store.load();
        List<Journal.Entry> entries = new ArrayList<>();
        if (recovered != null) {
            for (Journal.Entry e : recovered.entries()) {
                if (!changes(changes, e.ns, e.key) && Objects.equals(s.get(e.ns, e.key), e.set)) {
                    entries.add(e);
                }
            }
        }
        int first = entries.size();
        for (Change c : changes) {
            String current = s.get(c.ns, c.key);
            Journal.Entry old = recovered == null ? null : recovered.find(c.ns, c.key);
            String orig = old != null && Objects.equals(current, old.set) ? old.orig : current;
            entries.add(new Journal.Entry(c.ns, c.key, orig, c.next(current)));
        }
        Journal journal = new Journal(entries);
        store.save(journal);
        Session session = new Session(s, p, g, store, journal);
        try {
            g.arm(journal.toJson());
        } catch (RuntimeException e) {
            if (recovered == null) {
                store.clear(); // nothing changed; a recovered journal (now merged) must survive, though
            }
            throw e;
        }
        try {
            session.settingWritten = true; // set first: a throwing put may still have written
            for (int i = 0; i < changes.size(); i++) {
                changes.get(i).apply(s, entries.get(first + i).set);
            }
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

    private static boolean changes(List<Change> changes, String ns, String key) {
        for (Change c : changes) {
            if (c.ns.equals(ns) && c.key.equals(key)) {
                return true;
            }
        }
        return false;
    }

    public Journal journal() {
        return journal;
    }

    /** The session turned the panel off and has not closed (so it is to stay off). */
    public synchronized boolean panelOff() {
        return panelOff && !closed;
    }

    /**
     * Turns the panel back off after something (the unlock's wake-up) may have lit it, only while
     * this session still holds it off: under the same monitor as {@link #close}, so it can never
     * undo close's panel-on.
     */
    public synchronized void panelOffAgain() {
        if (panelOff && !closed) {
            power.setPanel(false);
        }
    }

    /**
     * Idempotent. Panel back on, then every journaled setting that still holds the value Lantern
     * wrote (even if the panel call throws an Error), then the persisted journal is removed; the
     * guard is disarmed last.
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
                    journal.restore(settings, log);
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
