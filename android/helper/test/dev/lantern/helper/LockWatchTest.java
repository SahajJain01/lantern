package dev.lantern.helper;

import java.io.IOException;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Deque;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Host-JVM tests for {@link LockWatch}: a stretch ends only on a successful read of unlocked
 * ({@code deviceLocked=0}); an unknown read keeps polling; closing the session never counts as an
 * unlock and drops the held PC copy. Fake {@link LockState.Source}s, no device.
 */
public final class LockWatchTest {
    static final long POLL_MS = 5;

    static String dump(int locked) {
        return " User \"Owner\" (id=0, flags=0x4c13) (current): trustManaged=0, deviceLocked=" + locked + "\n";
    }

    /** Sends and logs, recorded. */
    static final class Rec {
        final List<String> sent = Collections.synchronizedList(new ArrayList<>());
        final List<String> logs = Collections.synchronizedList(new ArrayList<>());

        LockWatch watch(LockState lock) {
            return new LockWatch(lock, m -> sent.add(m.toString()), logs::add, POLL_MS);
        }
    }

    /** A ClipSync holding a PC copy (the phone is locked), with its held writes recorded. */
    static ClipSync heldClip(List<String> heldWrites) {
        ClipSync c = new ClipSync(t -> { }, t -> { throw new AssertionError("normal write while locked"); },
            heldWrites::add, new LockState(() -> dump(1), s -> { }), r -> { });
        c.fromHost("pc copy");
        return c;
    }

    /** close() lands while a read is in flight; the read then returns "unlocked" anyway. */
    static void closeDuringReadIsNotAnUnlock() throws Exception {
        CountDownLatch inRead = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        AtomicInteger reads = new AtomicInteger();
        LockState lock = new LockState(() -> {
            reads.incrementAndGet();
            inRead.countDown();
            // Not interruptible: models a read that completes despite close()'s interrupt.
            boolean interrupted = false;
            while (true) {
                try {
                    release.await();
                    break;
                } catch (InterruptedException e) {
                    interrupted = true;
                }
            }
            if (interrupted) {
                Thread.currentThread().interrupt();
            }
            return dump(0);
        }, s -> { });
        Rec rec = new Rec();
        List<String> held = Collections.synchronizedList(new ArrayList<>());
        ClipSync clip = heldClip(held);
        LockWatch w = rec.watch(lock);
        AtomicInteger unlockedRuns = new AtomicInteger();
        w.onUnlocked = () -> {
            unlockedRuns.incrementAndGet();
            clip.unlocked();
        };
        w.onClosed = clip::dropHeld;
        w.onLocked("clipboard");
        eq(true, inRead.await(2, TimeUnit.SECONDS), "a read started");
        Thread poller = w.poller();
        w.close();
        release.countDown();
        poller.join(2000);
        eq(false, poller.isAlive(), "poller ended");
        eq(List.of("locked{reason=clipboard}"), rec.sent, "no unlocked after close");
        eq(0, unlockedRuns.get(), "onUnlocked not run");
        clip.unlocked(); // a follow-up already posted to lantern-clip would find nothing
        eq(List.of(), held, "held copy never written");
        eq(false, rec.logs.contains("lock: phone unlocked"), "no unlock logged: " + rec.logs);
    }

    /** close() interrupts a read that honours interrupts (the device dumpsys path): not an unlock. */
    static void closeInterruptingReadIsNotAnUnlock() throws Exception {
        CountDownLatch inRead = new CountDownLatch(1);
        LockState lock = new LockState(() -> {
            inRead.countDown();
            try {
                new CountDownLatch(1).await();
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new IOException("interrupted waiting for dumpsys trust");
            }
            return dump(1);
        }, s -> { });
        Rec rec = new Rec();
        List<String> held = Collections.synchronizedList(new ArrayList<>());
        ClipSync clip = heldClip(held);
        LockWatch w = rec.watch(lock);
        AtomicInteger unlockedRuns = new AtomicInteger();
        w.onUnlocked = () -> {
            unlockedRuns.incrementAndGet();
            clip.unlocked();
        };
        w.onClosed = clip::dropHeld;
        w.onLocked("open");
        eq(true, inRead.await(2, TimeUnit.SECONDS), "a read started");
        Thread poller = w.poller();
        w.close();
        poller.join(2000);
        eq(false, poller.isAlive(), "poller ended");
        eq(List.of("locked{reason=open}"), rec.sent, "no unlocked after close");
        eq(0, unlockedRuns.get(), "onUnlocked not run");
        clip.unlocked();
        eq(List.of(), held, "held copy never written");
    }

    /** Unknown reads (no current line, a failing dumpsys) mid-stretch keep polling. */
    static void unknownReadKeepsPolling() throws Exception {
        Deque<Object> script = new ArrayDeque<>(List.of(
            dump(1), "garbage\n", new IOException("dumpsys trust took over 3000 ms"),
            new IllegalStateException("boom"), dump(1), dump(0)));
        AtomicInteger reads = new AtomicInteger();
        LockState lock = new LockState(() -> {
            reads.incrementAndGet();
            Object o;
            synchronized (script) {
                o = script.isEmpty() ? dump(0) : script.poll();
            }
            if (o instanceof IOException) {
                throw (IOException) o;
            }
            if (o instanceof RuntimeException) {
                throw (RuntimeException) o;
            }
            return (String) o;
        }, s -> { });
        Rec rec = new Rec();
        List<String> held = Collections.synchronizedList(new ArrayList<>());
        ClipSync clip = heldClip(held);
        LockWatch w = rec.watch(lock);
        CountDownLatch unlocked = new CountDownLatch(1);
        w.onUnlocked = () -> {
            clip.unlocked();
            unlocked.countDown();
        };
        w.onLocked("clipboard");
        eq(true, unlocked.await(2, TimeUnit.SECONDS), "unlocked eventually");
        w.poller().join(2000);
        eq(6, reads.get(), "kept polling through the unknown reads, stopped at the first false");
        eq(List.of("locked{reason=clipboard}", "unlocked{}"), rec.sent, "one stretch, one unlocked");
        eq(List.of("pc copy"), held, "held copy written once, after the real unlock");
        w.close();
    }

    /** A read of unlocked ends the stretch exactly once; locks inside a stretch are deduped. */
    static void falseReadEndsTheStretchOnce() throws Exception {
        AtomicInteger reads = new AtomicInteger();
        LockState lock = new LockState(() -> {
            reads.incrementAndGet();
            return dump(0);
        }, s -> { });
        Rec rec = new Rec();
        LockWatch w = rec.watch(lock);
        AtomicInteger unlockedRuns = new AtomicInteger();
        w.onUnlocked = unlockedRuns::incrementAndGet;
        w.onLocked("clipboard");
        Thread p = w.poller();
        w.onLocked("open"); // same stretch: nothing sent
        p.join(2000);
        eq(false, p.isAlive(), "poller ended");
        Thread.sleep(10 * POLL_MS);
        eq(1, reads.get(), "polling stopped after the false read");
        eq(1, unlockedRuns.get(), "onUnlocked once");
        eq(List.of("locked{reason=clipboard}", "unlocked{}"), rec.sent, "locked then unlocked, once each");
        eq(true, rec.logs.contains("lock: phone unlocked"), "logged: " + rec.logs);
        // A later lock opens a new stretch.
        w.onLocked("open");
        w.poller().join(2000);
        eq(List.of("locked{reason=clipboard}", "unlocked{}", "locked{reason=open}", "unlocked{}"), rec.sent,
            "second stretch");
        w.close();
    }

    static void readIsTriState() {
        eq(Boolean.TRUE, new LockState(() -> dump(1), s -> { }).read(), "locked");
        eq(Boolean.FALSE, new LockState(() -> dump(0), s -> { }).read(), "unlocked");
        eq(null, new LockState(() -> "", s -> { }).read(), "no current line");
        eq(null, new LockState(() -> {
            throw new IOException("x");
        }, s -> { }).read(), "failing source");
    }

    static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"closeDuringReadIsNotAnUnlock", (Case) LockWatchTest::closeDuringReadIsNotAnUnlock},
            {"closeInterruptingReadIsNotAnUnlock", (Case) LockWatchTest::closeInterruptingReadIsNotAnUnlock},
            {"unknownReadKeepsPolling", (Case) LockWatchTest::unknownReadKeepsPolling},
            {"falseReadEndsTheStretchOnce", (Case) LockWatchTest::falseReadEndsTheStretchOnce},
            {"readIsTriState", (Case) LockWatchTest::readIsTriState},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   LockWatchTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL LockWatchTest." + c[0] + ": " + t);
            }
        }
        System.out.println("LockWatchTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
