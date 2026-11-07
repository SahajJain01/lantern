package dev.lantern.helper;

import java.nio.ByteBuffer;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.ReentrantLock;

/**
 * The encoder drain loop, kept free of Android classes so host tests drive it with fakes.
 *
 * <p>Each step dequeues one packet <b>under</b> the encoder lock (the {@link Source} copies the data
 * out and releases the codec buffer before returning), then calls the sink <b>outside</b> the lock, so
 * a sink that blocks (a socket the host stopped reading) never stalls {@code reconfigure} or
 * {@code close}. Packets copied before a reconfigure (older {@link Source#generation}) are dropped.
 *
 * <p>Errors: TRANSIENT retries with exponential back-off (5..200 ms) and becomes fatal after
 * {@link #maxTransient} in a row; RECOVERABLE calls the owner's {@link Recovery} outside the lock, up
 * to {@link #MAX_RECOVERIES} in a row without a delivered packet; FATAL (and a sink that throws) ends
 * the loop and reports once to the {@link FailureListener}. {@link #stop} ends it silently.
 */
final class DrainLoop {
    enum Fault { TRANSIENT, RECOVERABLE, FATAL }

    /** A packet copied out of the codec. {@code data} is only valid during the sink call. */
    static final class Packet {
        final int generation;
        final long ptsUs;
        final boolean config;
        final boolean key;
        final ByteBuffer data;

        Packet(int generation, long ptsUs, boolean config, boolean key, ByteBuffer data) {
            this.generation = generation;
            this.ptsUs = ptsUs;
            this.config = config;
            this.key = key;
            this.data = data;
        }
    }

    /** Returned by {@link Source#dequeue} when there is nothing to do and it did not block. */
    static final Packet IDLE = new Packet(-1, 0, false, false, null);

    interface Source {
        /**
         * Called with the lock held. Returns a copied packet, {@code null} if none arrived within the
         * dequeue timeout, or {@link #IDLE}. Throws on a codec error.
         */
        Packet dequeue() throws Exception;

        Fault classify(Exception e);

        /** Current codec generation (bumped by every reconfigure). Safe without the lock. */
        int generation();
    }

    /** Owner-side recovery (e.g. stop/configure/start and repoint the display). Runs unlocked. */
    interface Recovery {
        void recover() throws Exception;
    }

    interface FailureListener {
        void failed(String reason);
    }

    interface Logger {
        void log(String line);
    }

    static final long IDLE_MS = 10;
    static final long BACKOFF_MIN_MS = 5;
    static final long BACKOFF_MAX_MS = 200;
    static final int MAX_RECOVERIES = 3;
    /** Consecutive transient errors before giving up (~5 s with the back-off above). */
    int maxTransient = 30;
    /** Encoder points this at logcat; host tests keep stderr. */
    Logger logger = System.err::println;

    private final ReentrantLock lock;
    private final Source source;
    private final VideoSink sink;
    private final int win;
    private final Recovery recovery;
    private final FailureListener listener;
    private final AtomicBoolean reported = new AtomicBoolean();
    private volatile boolean stopped;

    DrainLoop(ReentrantLock lock, Source source, VideoSink sink, int win, Recovery recovery,
            FailureListener listener) {
        this.lock = lock;
        this.source = source;
        this.sink = sink;
        this.win = win;
        this.recovery = recovery;
        this.listener = listener;
    }

    /** Ends the loop without a failure report. Does not wait for it. */
    void stop() {
        stopped = true;
    }

    boolean stopped() {
        return stopped;
    }

    /** Blocking; returns when stopped or after a reported failure. */
    void run() {
        int transients = 0;
        int recoveries = 0;
        long backoff = BACKOFF_MIN_MS;
        while (!stopped) {
            Packet p;
            Exception error = null;
            lock.lock();
            try {
                if (stopped) {
                    return;
                }
                p = source.dequeue();
            } catch (Exception e) {
                p = null;
                error = e;
            } finally {
                lock.unlock();
            }
            if (stopped) {
                return;
            }
            if (error != null) {
                Fault fault = source.classify(error);
                if (fault == Fault.TRANSIENT && ++transients < maxTransient) {
                    logger.log("drain win=" + win + ": transient codec error (" + error + "), retry in "
                        + backoff + " ms");
                    if (!sleep(backoff)) {
                        return;
                    }
                    backoff = Math.min(BACKOFF_MAX_MS, backoff * 2);
                    continue;
                }
                if (fault == Fault.RECOVERABLE && recovery != null && recoveries < MAX_RECOVERIES) {
                    recoveries++;
                    logger.log("drain win=" + win + ": recoverable codec error (" + error + "), recovering ("
                        + recoveries + "/" + MAX_RECOVERIES + ")");
                    try {
                        recovery.recover();
                        continue;
                    } catch (Exception re) {
                        fail("codec recovery failed: " + re + " (after " + error + ")");
                        return;
                    }
                }
                String what = fault == Fault.TRANSIENT ? "codec error persisted " + transients + "x"
                    : fault == Fault.RECOVERABLE ? "codec error, recovery exhausted" : "fatal codec error";
                fail(what + ": " + error);
                return;
            }
            transients = 0;
            backoff = BACKOFF_MIN_MS;
            if (p == IDLE) {
                if (!sleep(IDLE_MS)) {
                    return;
                }
                continue;
            }
            if (p == null || p.generation != source.generation()) {
                continue; // timeout, or a packet from before a reconfigure
            }
            recoveries = 0;
            try {
                sink.packet(win, p.ptsUs, p.config, p.key, p.data);
            } catch (RuntimeException e) {
                if (!stopped) {
                    fail("sink: " + e);
                }
                return;
            }
        }
    }

    private boolean sleep(long ms) {
        try {
            Thread.sleep(ms);
            return true;
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }
    }

    private void fail(String reason) {
        if (stopped || !reported.compareAndSet(false, true)) {
            return;
        }
        logger.log("drain win=" + win + " failed: " + reason);
        if (listener != null) {
            listener.failed(reason);
        }
    }
}
