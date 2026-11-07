package dev.lantern.helper;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.ReentrantLock;

/** Host-JVM tests for {@link DrainLoop}: error classification, back-off, sink outside the lock. */
public final class DrainLoopTest {
    /** Scripted fake codec. Each dequeue runs the next action; after the script, repeats the last. */
    static final class FakeSource implements DrainLoop.Source {
        interface Action {
            DrainLoop.Packet run() throws Exception;
        }

        final List<Action> script = new ArrayList<>();
        final AtomicInteger calls = new AtomicInteger();
        volatile int generation;
        volatile boolean lockHeld = true;
        final ReentrantLock lock;

        FakeSource(ReentrantLock lock) {
            this.lock = lock;
        }

        @Override
        public DrainLoop.Packet dequeue() throws Exception {
            lockHeld &= lock.isHeldByCurrentThread();
            int i = calls.getAndIncrement();
            return script.get(Math.min(i, script.size() - 1)).run();
        }

        @Override
        public DrainLoop.Fault classify(Exception e) {
            if (e instanceof Transient) {
                return DrainLoop.Fault.TRANSIENT;
            }
            if (e instanceof Recoverable) {
                return DrainLoop.Fault.RECOVERABLE;
            }
            return DrainLoop.Fault.FATAL;
        }

        @Override
        public int generation() {
            return generation;
        }
    }

    static final class Transient extends Exception {
    }

    static final class Recoverable extends Exception {
    }

    static DrainLoop.Packet packet(int gen, byte b) {
        return new DrainLoop.Packet(gen, 1000, false, true, ByteBuffer.wrap(new byte[] {b}));
    }

    static final class Failures implements DrainLoop.FailureListener {
        final List<String> reasons = new ArrayList<>();

        @Override
        public synchronized void failed(String reason) {
            reasons.add(reason);
        }

        synchronized int count() {
            return reasons.size();
        }
    }

    static Thread start(DrainLoop loop) {
        Thread t = new Thread(loop::run, "test-drain");
        t.setDaemon(true);
        t.start();
        return t;
    }

    /** A sink that never returns must not keep stop+lock (Encoder.close) from finishing in ~2 s. */
    static void blockingSinkDoesNotBlockClose() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> packet(0, (byte) 1));
        CountDownLatch inSink = new CountDownLatch(1);
        CountDownLatch never = new CountDownLatch(1);
        VideoSink sink = (win, pts, config, key, data) -> {
            inSink.countDown();
            try {
                never.await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        };
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, sink, 7, null, f);
        Thread t = start(loop);
        check(inSink.await(2, TimeUnit.SECONDS), "sink reached");
        long t0 = System.nanoTime();
        loop.stop();
        boolean got = lock.tryLock(2, TimeUnit.SECONDS);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(got, "close acquired the encoder lock while the sink blocks (" + ms + " ms)");
        check(ms < 2000, "close within 2 s, took " + ms);
        lock.unlock();
        check(src.lockHeld, "dequeue ran under the lock");
        eq(0, f.count(), "stopping is not a failure");
        t.interrupt();
        never.countDown();
        t.join(2000);
        check(!t.isAlive(), "drain exits once the sink returns after stop");
    }

    /** A fatal codec error reaches the listener exactly once and ends the loop. */
    static void fatalReportsOnceAndStops() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> packet(0, (byte) 1));
        src.script.add(() -> {
            throw new IllegalStateException("codec reclaimed");
        });
        List<Byte> got = new ArrayList<>();
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> got.add(d.get(0)), 3, null, f);
        Thread t = start(loop);
        t.join(2000);
        check(!t.isAlive(), "loop ended on fatal error");
        eq(1, f.count(), "one failure report");
        check(f.reasons.get(0).contains("codec reclaimed"), "reason names the error: " + f.reasons.get(0));
        eq(2, src.calls.get(), "no dequeue after the fatal error");
        eq(1, got.size(), "packet before the error delivered");
    }

    /** Transient errors back off (no hot spin) and the stream continues without a report. */
    static void transientBacksOffThenContinues() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        for (int i = 0; i < 4; i++) {
            src.script.add(() -> {
                throw new Transient();
            });
        }
        CountDownLatch delivered = new CountDownLatch(1);
        src.script.add(() -> packet(0, (byte) 9));
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> delivered.countDown(), 1, null, f);
        long t0 = System.nanoTime();
        Thread t = start(loop);
        check(delivered.await(3, TimeUnit.SECONDS), "packet after transient errors");
        long ms = (System.nanoTime() - t0) / 1_000_000;
        // back-off 5+10+20+40 ms before the 5th dequeue
        check(ms >= 70, "backed off between transient retries, only " + ms + " ms");
        loop.stop();
        t.join(2000);
        eq(0, f.count(), "transient errors are not reported");
    }

    /** A codec stuck in transient errors is not polled in a hot loop and eventually escalates. */
    static void persistentTransientNoHotSpinThenFails() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> {
            throw new Transient();
        });
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> { }, 1, null, f);
        loop.maxTransient = 6;
        Thread t = start(loop);
        Thread.sleep(150);
        int at150 = src.calls.get();
        check(at150 <= 6, "no hot spin: " + at150 + " dequeues in 150 ms");
        t.join(3000);
        check(!t.isAlive(), "escalated to fatal and stopped");
        eq(1, f.count(), "one report after persistent transient errors");
        eq(6, src.calls.get(), "dequeues == maxTransient");
    }

    /** No codec (failed recreate) is idle, not a busy loop: the source returns IDLE without blocking. */
    static void idleSleeps() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> DrainLoop.IDLE);
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> { }, 1, null, f);
        Thread t = start(loop);
        Thread.sleep(200);
        loop.stop();
        t.join(2000);
        int n = src.calls.get();
        check(n <= 25, "idle polls slept: " + n + " in 200 ms");
        eq(0, f.count(), "idle is not a failure");
    }

    /** Recoverable errors call the owner's recovery outside the lock; repeated ones escalate. */
    static void recoverableRecoversThenEscalates() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> {
            throw new Recoverable();
        });
        AtomicInteger recoveries = new AtomicInteger();
        boolean[] recoveredUnlocked = {true};
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> { }, 1, () -> {
            recoveredUnlocked[0] &= !lock.isHeldByCurrentThread();
            recoveries.incrementAndGet();
        }, f);
        Thread t = start(loop);
        t.join(3000);
        check(!t.isAlive(), "stopped after repeated recoverable errors");
        eq(DrainLoop.MAX_RECOVERIES, recoveries.get(), "recoveries attempted");
        check(recoveredUnlocked[0], "recovery ran outside the encoder lock");
        eq(1, f.count(), "one report");
    }

    /** A sink that throws ends the loop and is reported once. */
    static void sinkFailureReported() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> packet(0, (byte) 1));
        Failures f = new Failures();
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> {
            throw new IllegalStateException("socket closed");
        }, 1, null, f);
        Thread t = start(loop);
        t.join(2000);
        check(!t.isAlive(), "loop ended on sink failure");
        eq(1, f.count(), "one report");
        check(f.reasons.get(0).startsWith("sink:"), "reason: " + f.reasons.get(0));
    }

    /** A packet copied before a reconfigure (older generation) is dropped, not delivered. */
    static void staleGenerationDropped() throws Exception {
        ReentrantLock lock = new ReentrantLock(true);
        FakeSource src = new FakeSource(lock);
        src.script.add(() -> {
            DrainLoop.Packet p = packet(0, (byte) 1);
            src.generation = 1; // reconfigure lands between copy and sink
            return p;
        });
        src.script.add(() -> packet(1, (byte) 2));
        List<Byte> got = new ArrayList<>();
        CountDownLatch one = new CountDownLatch(1);
        DrainLoop loop = new DrainLoop(lock, src, (w, p, c, k, d) -> {
            synchronized (got) {
                got.add(d.get(0));
            }
            one.countDown();
        }, 1, null, new Failures());
        Thread t = start(loop);
        check(one.await(2, TimeUnit.SECONDS), "a packet delivered");
        loop.stop();
        t.join(2000);
        synchronized (got) {
            eq((byte) 2, got.get(0), "first delivered packet is from the new generation");
        }
    }

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    private static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"blockingSinkDoesNotBlockClose", (Case) DrainLoopTest::blockingSinkDoesNotBlockClose},
            {"fatalReportsOnceAndStops", (Case) DrainLoopTest::fatalReportsOnceAndStops},
            {"transientBacksOffThenContinues", (Case) DrainLoopTest::transientBacksOffThenContinues},
            {"persistentTransientNoHotSpinThenFails", (Case) DrainLoopTest::persistentTransientNoHotSpinThenFails},
            {"idleSleeps", (Case) DrainLoopTest::idleSleeps},
            {"recoverableRecoversThenEscalates", (Case) DrainLoopTest::recoverableRecoversThenEscalates},
            {"sinkFailureReported", (Case) DrainLoopTest::sinkFailureReported},
            {"staleGenerationDropped", (Case) DrainLoopTest::staleGenerationDropped},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   DrainLoopTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL DrainLoopTest." + c[0] + ": " + t);
            }
        }
        System.out.println("DrainLoopTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
