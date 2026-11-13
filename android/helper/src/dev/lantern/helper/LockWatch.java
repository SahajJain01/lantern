package dev.lantern.helper;

import java.util.function.Consumer;

/**
 * One locked stretch at a time: the first {@link #onLocked} sends {@code locked{reason}} and
 * starts a {@code lantern-lock} thread that reads {@link LockState#read} every poll interval. The
 * stretch ends only on a successful read of unlocked ({@code deviceLocked=0}): then it sends
 * {@code unlocked}, runs {@link #onUnlocked} (writes the held PC copy) and stops. An unknown read
 * (no current user line, a failing or slow {@code dumpsys}) keeps polling. Later calls inside the
 * stretch do nothing.
 *
 * <p>Closing (session end) is never an unlock: a read that returns after {@link #close} is
 * ignored, and {@link #onClosed} drops the held PC copy so it can't land on a locked phone.
 */
final class LockWatch implements AutoCloseable {
    static final long POLL_MS = 2000;

    private final LockState lock;
    private final Consumer<Msg> send;
    private final Consumer<String> log;
    private final long pollMs;
    /** Set before the session is up. */
    volatile Runnable onUnlocked = () -> { };
    /** Run once by {@link #close}: drops the held PC copy. Set before the session is up. */
    volatile Runnable onClosed = () -> { };
    /** Guarded by this. */
    private boolean stretch;
    /** Guarded by this. */
    private boolean closed;
    /** The current stretch's poll thread. Guarded by this. */
    private Thread poller;

    /** @param send {@code router::sendSession} */
    LockWatch(LockState lock, Consumer<Msg> send, Consumer<String> log, long pollMs) {
        this.lock = lock;
        this.send = send;
        this.log = log;
        this.pollMs = pollMs;
    }

    void onLocked(String reason) {
        Thread t;
        synchronized (this) {
            if (stretch || closed) {
                return;
            }
            stretch = true;
            t = new Thread(this::poll, "lantern-lock");
            t.setDaemon(true);
            poller = t;
            // Under the monitor, so a stretch's unlocked can't overtake its locked.
            log.accept("lock: phone locked (" + reason + ")");
            send.accept(Msg.of("locked").put("reason", reason));
        }
        t.start();
    }

    /** Tests: the current (or last) stretch's poll thread, or null. */
    synchronized Thread poller() {
        return poller;
    }

    private void poll() {
        while (true) {
            try {
                Thread.sleep(pollMs);
            } catch (InterruptedException e) {
                return;
            }
            synchronized (this) {
                if (closed || ended()) {
                    return;
                }
            }
            Boolean locked = lock.read();
            synchronized (this) {
                // A read interrupted by close() (or finishing after it) is not an unlock; a stretch
                // already ended by unlockConfirmed() sends nothing more.
                if (closed || Thread.currentThread().isInterrupted() || ended()) {
                    return;
                }
                if (!Boolean.FALSE.equals(locked)) {
                    continue; // still locked, or unknown: keep polling
                }
                // Under the monitor: close() either came first (returned above) or waits until
                // the follow-up is posted, and then drops the held copy before it can be written
                // to a phone that is no longer served.
                log.accept("lock: phone unlocked");
                send.accept(Msg.of("unlocked"));
                try {
                    onUnlocked.run();
                } catch (RuntimeException e) {
                    log.accept("lock: unlock follow-up failed: " + e);
                }
                stretch = false; // after unlocked went out, so a new stretch's locked follows it
                return;
            }
        }
    }

    /** The calling poller's stretch is over (ended by {@link #unlockConfirmed}). Under the monitor. */
    private boolean ended() {
        return !stretch || poller != Thread.currentThread();
    }

    /**
     * The Unlocker saw {@code deviceLocked=0} after typing the PIN: end the stretch now instead of
     * at the next poll. Sends {@code unlocked} and runs {@link #onUnlocked} (writes the held PC
     * copy) once; the stretch's poller then stops without sending. Does nothing with no stretch
     * running (its poller already sent {@code unlocked}, or no {@code locked} was sent) or after
     * {@link #close}.
     */
    void unlockConfirmed() {
        synchronized (this) {
            if (closed || !stretch) {
                return;
            }
            log.accept("lock: phone unlocked (PIN)");
            send.accept(Msg.of("unlocked"));
            try {
                onUnlocked.run();
            } catch (RuntimeException e) {
                log.accept("lock: unlock follow-up failed: " + e);
            }
            stretch = false; // the poller sees ended() and returns; a new lock opens a new stretch
        }
    }

    @Override
    public void close() {
        Thread t;
        synchronized (this) {
            if (closed) {
                return;
            }
            closed = true;
            t = poller;
        }
        if (t != null) {
            t.interrupt();
        }
        try {
            onClosed.run();
        } catch (RuntimeException e) {
            log.accept("lock: close follow-up failed: " + e);
        }
    }
}
