package dev.lantern.helper;

import java.util.Arrays;
import java.util.List;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.Executor;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;
import java.util.function.Predicate;

/**
 * The session's keyboard switch: enable the companion's Lantern IME and select it, both journaled
 * by {@link Session} (restored by the helper, Cleanup, or the companion's watchdog). Also the
 * helper's relay of a window's {@code ime} to the companion. Plain Java.
 */
final class ImeSetup {
    /** The companion's hidden IME. */
    static final String ID = "dev.lantern.companion/.LanternIme";

    private ImeSetup() {
    }

    /**
     * {@code list} ({@code secure/enabled_input_methods}: {@code :}-separated
     * {@code component[;subtype...]} entries) with {@code id} enabled: {@code id} when the list is
     * null or empty, the list unchanged when an entry's component is {@code id}, otherwise the list
     * plus {@code ":" + id}.
     */
    static String withEnabled(String list, String id) {
        if (list == null || list.isEmpty()) {
            return id;
        }
        for (String entry : list.split(":")) {
            int semi = entry.indexOf(';');
            String component = semi < 0 ? entry : entry.substring(0, semi);
            if (component.equals(id)) {
                return list;
            }
        }
        return list + ":" + id;
    }

    /** Enable, then select, the Lantern IME. They go after {@link Session#STAY_ON}. */
    static List<Change> changes() {
        return Arrays.asList(
            Change.setting("secure", "enabled_input_methods", l -> withEnabled(l, ID)),
            Change.ime(ID));
    }

    /**
     * The keyboard the companion's watchdog restores if the link is lost ({@code ime_prev}): the
     * journal's original {@code default_input_method}, or {@code ""} when there is none or it is
     * Lantern's own (left behind by an unrestored session).
     */
    static String prevIme(Journal journal) {
        Journal.Entry e = journal.find(Journal.IME_NS, Journal.IME_KEY);
        if (e == null || e.orig == null || e.orig.equals(ID)) {
            return "";
        }
        return e.orig;
    }

    /**
     * Starts a {@link Session} with {@code changes}. Each call must arm a fresh guard (on the device
     * a new {@code Cleanup.spawnGuard}): a guard is single-use.
     */
    interface Starter {
        Session start(List<Change> changes);
    }

    /** The session {@link #start} brought up, and whether the Lantern keyboard is selected in it. */
    static final class Started {
        final Session session;
        final boolean imeSelected;
        /** {@code "keyboard switch failed: <reason>"} when the keyboard was tried and fell back; else null. */
        final String imeFailure;

        Started(Session session, boolean imeSelected, String imeFailure) {
            this.session = session;
            this.imeSelected = imeSelected;
            this.imeFailure = imeFailure;
        }
    }

    /**
     * The session start (Ruling 15). Without {@code withIme}: {@link Session#STAY_ON} only. With it:
     * STAY_ON plus {@link #changes()}; if that start fails and its own rollback succeeded (no
     * suppressed close failure: every setting is back and that attempt's guard is disarmed), the
     * keyboard is optional, so it starts again with STAY_ON only and reports the reason. A failed
     * rollback is rethrown untouched: the phone may be mid-restore, which Cleanup and the journal
     * own. A failure of the STAY_ON-only start is rethrown too.
     */
    static Started start(boolean withIme, Starter starter, Consumer<String> log) {
        List<Change> stayOn = java.util.Collections.singletonList(Session.STAY_ON);
        if (!withIme) {
            return new Started(starter.start(stayOn), false, null);
        }
        List<Change> all = new java.util.ArrayList<>();
        all.add(Session.STAY_ON);
        all.addAll(changes());
        try {
            return new Started(starter.start(all), true, null);
        } catch (RuntimeException e) {
            if (e.getSuppressed().length > 0) {
                throw e;
            }
            String reason = e.getMessage() != null ? e.getMessage() : e.toString();
            log.accept("serve: keyboard switch failed (" + e + "), session without the Lantern keyboard");
            return new Started(starter.start(stayOn), false, "keyboard switch failed: " + reason);
        }
    }

    /** The {@code companion} message for lanternd once the session is up. */
    static Msg companionState(boolean linked, String imeFailure) {
        if (!linked) {
            return Msg.of("companion").put("state", "unavailable").put("msg", "companion did not connect");
        }
        if (imeFailure != null) {
            return Msg.of("companion").put("state", "unavailable").put("msg", imeFailure);
        }
        return Msg.of("companion").put("state", "ready");
    }

    /** How many {@code ime} messages may wait for a slow companion before new ones are dropped. */
    static final int RELAY_QUEUE = 256;

    /**
     * {@link Router.Extras#ime} on the device: forwards {@code ime{op, text}} to the companion, in
     * order, from its own thread, so the router's control thread never waits on a companion that
     * stopped reading. A message is dropped while the companion is not linked, or when
     * {@link #RELAY_QUEUE} are already waiting; each such stretch is logged once (not once per
     * frame). Never logs the text.
     */
    static final class Relay implements Router.Extras {
        private final Predicate<Msg> send;
        private final Consumer<String> log;
        private final Executor exec;
        /** Guarded by this: a drop was already logged since the last delivered message. */
        private boolean dropLogged;

        /** One sender thread with a queue of {@code capacity}; a full queue rejects. Daemon thread. */
        static ExecutorService executor(int capacity) {
            return new ThreadPoolExecutor(1, 1, 0, TimeUnit.MILLISECONDS, new ArrayBlockingQueue<>(capacity), r -> {
                Thread t = new Thread(r, "lantern-ime-relay");
                t.setDaemon(true);
                return t;
            }, new ThreadPoolExecutor.AbortPolicy());
        }

        /**
         * @param send delivers to the linked companion; false if none is linked or the write fails
         * @param exec runs the sends in order (on the device {@link #executor}{@code (RELAY_QUEUE)})
         */
        Relay(Predicate<Msg> send, Consumer<String> log, Executor exec) {
            this.send = send;
            this.log = log;
            this.exec = exec;
        }

        @Override
        public void ime(String op, String text) {
            Msg m = Msg.of("ime").put("op", op).put("text", text);
            try {
                exec.execute(() -> {
                    boolean ok;
                    try {
                        ok = send.test(m);
                    } catch (RuntimeException e) {
                        ok = false;
                    }
                    dropped(ok, "companion not linked, dropping ime messages until it is");
                });
            } catch (RejectedExecutionException e) {
                dropped(false, "companion is not keeping up (or the session is ending), dropping ime messages");
            }
        }

        private void dropped(boolean delivered, String why) {
            synchronized (this) {
                if (delivered) {
                    dropLogged = false;
                    return;
                }
                if (dropLogged) {
                    return;
                }
                dropLogged = true;
            }
            log.accept("ime: " + why);
        }

        @Override
        public void clip(String text) {
            // Not a companion message: Main's extras send clip to ClipSync instead.
        }
    }
}
