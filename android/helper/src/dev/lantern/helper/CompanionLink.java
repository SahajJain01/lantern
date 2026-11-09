package dev.lantern.helper;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.Closeable;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;

/**
 * The helper's side of the companion link (docs/protocol.md, "Companion link"). Plain Java: on the
 * device the {@link Listener} is a {@link HandoffListener} fed with pipe pairs that
 * {@link DeviceCompanion.Handoff} (the {@link Broadcaster}) also hands to the companion's provider.
 *
 * <p>{@link #connect} tells the companion where to connect and the link token, then waits for
 * an authenticated {@code hello}. Accepting goes on in the background until {@link #close}: a
 * connection whose first frame is not a {@code hello} with the right version and token is closed,
 * and a newly authenticated companion replaces the linked one.
 *
 * <p>Threads: one accept thread, one reader thread per connection, one timer that closes
 * connections that stay silent past {@value #HELLO_TIMEOUT_MS} ms.
 */
public final class CompanionLink implements Closeable {
    /** How long an accepted connection may take to send its hello. */
    static final long HELLO_TIMEOUT_MS = 2000;

    /** Companion messages relayed to lanternd; every other {@code t} is logged and dropped. */
    static final Set<String> RELAYED = Collections.unmodifiableSet(new HashSet<>(Arrays.asList(
        "app", "apps_end", "app_removed")));

    /** Where companion connections come from. */
    public interface Listener {
        /** What the {@link Broadcaster} is given as {@code socket} (informational on the device). */
        String name();

        Conn accept() throws IOException;

        /** Idempotent; should unblock {@link #accept}. */
        void close();
    }

    /** One accepted connection. */
    public interface Conn extends Closeable {
        InputStream in() throws IOException;

        OutputStream out() throws IOException;
    }

    /**
     * Starts one link attempt and gives the companion the token (on the device: a pipe pair offered
     * to the listener and handed to the companion's provider). Throws if the companion is missing.
     */
    public interface Broadcaster {
        void link(String socket, String token) throws IOException;
    }

    private final class Peer {
        final Conn conn;
        final DataInputStream in;
        final DataOutputStream out;

        Peer(Conn conn) throws IOException {
            this.conn = conn;
            in = new DataInputStream(new BufferedInputStream(conn.in(), 1 << 16));
            out = new DataOutputStream(new BufferedOutputStream(conn.out(), 1 << 16));
        }

        boolean write(Msg m) {
            try {
                Proto.write(out, Proto.KIND_CONTROL, 0, codec.encode(m));
                return true;
            } catch (IOException e) {
                log.accept("companion: send " + m.t + " failed: " + e);
                return false;
            }
        }

        void close() {
            try {
                conn.close();
            } catch (IOException | RuntimeException ignored) {
                // already closed
            }
        }
    }

    private final Listener listener;
    private final Broadcaster broadcaster;
    private final ControlCodec codec;
    private final String token;
    private final Consumer<Msg> toDaemon;
    private final Consumer<String> log;
    private final Object lock = new Object();
    private final Set<Peer> pending = new HashSet<>();
    private final ScheduledExecutorService timer = Executors.newSingleThreadScheduledExecutor(r -> {
        Thread t = new Thread(r, "lantern-companion-timer");
        t.setDaemon(true);
        return t;
    });
    private Peer current;
    private String prevIme;
    private boolean started;
    private boolean closed;

    public CompanionLink(Listener listener, Broadcaster broadcaster, ControlCodec codec, String token,
            Consumer<Msg> toDaemon, Consumer<String> log) {
        this.listener = listener;
        this.broadcaster = broadcaster;
        this.codec = codec;
        this.token = token;
        this.toDaemon = toDaemon;
        this.log = log;
    }

    /**
     * Starts accepting, broadcasts, and waits up to {@code timeoutMs} for an authenticated
     * companion. False on timeout or a failed broadcast; the session goes on without it (a
     * companion that links later is still taken).
     */
    public boolean connect(long timeoutMs) {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMs);
        synchronized (lock) {
            if (closed) {
                return false;
            }
            if (!started) {
                started = true;
                Thread t = new Thread(this::acceptLoop, "lantern-companion-accept");
                t.setDaemon(true);
                t.start();
            }
        }
        try {
            broadcaster.link(listener.name(), token);
        } catch (IOException | RuntimeException e) {
            log.accept("companion: broadcast failed: " + e);
            return false;
        }
        synchronized (lock) {
            while (current == null && !closed) {
                long left = TimeUnit.NANOSECONDS.toMillis(deadline - System.nanoTime());
                if (left <= 0) {
                    log.accept("companion: no link after " + timeoutMs + " ms");
                    return false;
                }
                try {
                    lock.wait(left);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return false;
                }
            }
            return current != null;
        }
    }

    /**
     * Stores the keyboard to restore if the link is lost and sends it as {@code ime_prev{id}} to the
     * linked companion; a companion that links later gets it right after its hello reply.
     */
    public void setPrevIme(String id) {
        synchronized (lock) {
            prevIme = id == null ? "" : id;
            if (current != null) {
                current.write(imePrev());
            }
        }
    }

    /** Sends {@code m} to the linked companion. False if none is linked or the write fails. */
    public boolean send(Msg m) {
        Peer p;
        synchronized (lock) {
            p = current;
        }
        return p != null && p.write(m);
    }

    public boolean alive() {
        synchronized (lock) {
            return current != null;
        }
    }

    @Override
    public void close() {
        Peer[] peers;
        synchronized (lock) {
            if (closed) {
                return;
            }
            closed = true;
            peers = pending.toArray(new Peer[0]);
            pending.clear();
            if (current != null) {
                peers = Arrays.copyOf(peers, peers.length + 1);
                peers[peers.length - 1] = current;
                current = null;
            }
            lock.notifyAll();
        }
        listener.close();
        for (Peer p : peers) {
            p.close();
        }
        timer.shutdownNow();
    }

    private Msg imePrev() {
        return Msg.of("ime_prev").put("id", prevIme);
    }

    private void acceptLoop() {
        while (true) {
            Conn c;
            try {
                c = listener.accept();
            } catch (IOException | RuntimeException e) {
                synchronized (lock) {
                    if (!closed) {
                        log.accept("companion: accept failed: " + e);
                    }
                }
                return;
            }
            Peer p;
            try {
                p = new Peer(c);
            } catch (IOException e) {
                log.accept("companion: connection setup failed: " + e);
                try {
                    c.close();
                } catch (IOException ignored) {
                    // dropped anyway
                }
                continue;
            }
            synchronized (lock) {
                if (closed) {
                    p.close();
                    return;
                }
                pending.add(p);
            }
            try {
                timer.schedule(() -> {
                    synchronized (lock) {
                        if (!pending.remove(p)) {
                            return; // authenticated or already gone
                        }
                    }
                    log.accept("companion: no hello within " + HELLO_TIMEOUT_MS + " ms, closed");
                    p.close();
                }, HELLO_TIMEOUT_MS, TimeUnit.MILLISECONDS);
            } catch (RejectedExecutionException e) {
                p.close(); // closing
                return;
            }
            Thread t = new Thread(() -> serve(p), "lantern-companion-conn");
            t.setDaemon(true);
            t.start();
        }
    }

    private void serve(Peer p) {
        try {
            if (!authenticate(p)) {
                synchronized (lock) {
                    pending.remove(p);
                }
                p.close();
                return;
            }
            Peer old;
            synchronized (lock) {
                if (closed || !pending.remove(p)) {
                    p.close(); // closed, or the hello timer already dropped it
                    return;
                }
                // Under the lock, so no other message can precede the hello reply and ime_prev.
                if (!p.write(Msg.of("hello").put("version", Proto.VERSION).put("token", ""))) {
                    p.close();
                    return;
                }
                if (prevIme != null) {
                    p.write(imePrev());
                }
                old = current;
                current = p;
                lock.notifyAll();
            }
            if (old != null) {
                log.accept("companion: replaced by a new link");
                old.close();
            }
            log.accept("companion linked");
            readLoop(p);
        } finally {
            boolean lost;
            synchronized (lock) {
                lost = current == p;
                if (lost) {
                    current = null;
                }
            }
            p.close();
            if (lost) {
                log.accept("companion: link lost");
            }
        }
    }

    /** True if the first frame is a hello with this protocol's version and the link token. */
    private boolean authenticate(Peer p) {
        try {
            Proto.Frame f = Proto.read(p.in);
            if (f == null) {
                log.accept("companion: EOF before hello");
                return false;
            }
            if (f.kind != Proto.KIND_CONTROL || f.stream != 0) {
                log.accept("companion: first frame is not control on stream 0, closed");
                return false;
            }
            Msg m = codec.decode(f.payload);
            if (!m.t.equals("hello")) {
                log.accept("companion: first message is " + m.t + ", not hello, closed");
                return false;
            }
            if (m.num("version") != Proto.VERSION) {
                log.accept("companion: version " + m.num("version") + " != " + Proto.VERSION + ", closed");
                return false;
            }
            byte[] got = m.str("token").getBytes(StandardCharsets.UTF_8);
            if (!MessageDigest.isEqual(got, token.getBytes(StandardCharsets.UTF_8))) {
                log.accept("companion: bad link token, closed");
                return false;
            }
            return true;
        } catch (IOException | RuntimeException e) {
            log.accept("companion: bad hello: " + e);
            return false;
        }
    }

    private void readLoop(Peer p) {
        try {
            while (true) {
                Proto.Frame f = Proto.read(p.in);
                if (f == null) {
                    return;
                }
                if (f.kind != Proto.KIND_CONTROL || f.stream != 0) {
                    log.accept("companion: frame kind " + f.kind + " stream " + f.stream + " dropped");
                    continue;
                }
                Msg m;
                try {
                    m = codec.decode(f.payload);
                } catch (IOException e) {
                    log.accept("companion: undecodable message dropped: " + e.getMessage());
                    continue;
                }
                if (RELAYED.contains(m.t)) {
                    try {
                        toDaemon.accept(m);
                    } catch (RuntimeException e) {
                        log.accept("companion: relay of " + m.t + " failed: " + e);
                    }
                } else {
                    log.accept("companion: unexpected " + m.t + " dropped"); // never log fields (clip text)
                }
            }
        } catch (IOException e) {
            synchronized (lock) {
                if (closed || current != p) {
                    return; // closed under us
                }
            }
            log.accept("companion: read failed: " + e);
        }
    }
}
