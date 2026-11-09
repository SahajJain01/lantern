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
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;
import java.util.function.IntPredicate;

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
 *
 * <p>Locking: the link lock guards state only and is never held while writing. Writes to a peer
 * are serialized on that peer's stream ({@link Proto#write} locks it), so a companion that stops
 * reading blocks only its writers, never {@link #close}, {@link #alive} or {@link #connect}; close
 * releases them.
 */
public final class CompanionLink implements Closeable {
    /** How long an accepted connection may take to send its hello. */
    static final long HELLO_TIMEOUT_MS = 2000;

    /** Companion messages relayed to lanternd; every other {@code t} is logged and dropped. */
    static final Set<String> RELAYED = Collections.unmodifiableSet(new HashSet<>(Arrays.asList(
        "app", "apps_end", "app_removed")));

    /** Base64 length of a {@link Proto#MAX_ICON}-byte PNG (padded, no line breaks). */
    static final int MAX_ICON_B64 = (Proto.MAX_ICON + 2) / 3 * 4;

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
    private final IntPredicate managedProfile;
    /** Whether a user is a managed profile, asked once per user (a profile's type never changes). */
    private final Map<Integer, Boolean> managed = new ConcurrentHashMap<>();
    /** Launcher labels from the catalogue, keyed {@code pkg + ":" + user}. */
    private final Map<String, String> labels = new ConcurrentHashMap<>();
    private Peer current;
    private String prevIme;
    /** Set by {@link #startSync}: every linked companion (now or later) is told to {@code sync}. */
    private boolean syncWanted;
    private boolean started;
    private boolean closed;

    public CompanionLink(Listener listener, Broadcaster broadcaster, ControlCodec codec, String token,
            Consumer<Msg> toDaemon, Consumer<String> log) {
        this(listener, broadcaster, codec, token, toDaemon, log, u -> false);
    }

    /**
     * @param managedProfile whether an Android user is a managed (work) profile: relayed {@code app}
     *     messages for it get {@code work:true}. Must not throw (on the device: false on any error).
     */
    public CompanionLink(Listener listener, Broadcaster broadcaster, ControlCodec codec, String token,
            Consumer<Msg> toDaemon, Consumer<String> log, IntPredicate managedProfile) {
        this.listener = listener;
        this.broadcaster = broadcaster;
        this.codec = codec;
        this.token = token;
        this.toDaemon = toDaemon;
        this.log = log;
        this.managedProfile = managedProfile;
    }

    /** The catalogue's launcher label of {@code pkg} in {@code user}, or null if unknown. */
    public String label(String pkg, int user) {
        return labels.get(pkg + ":" + user);
    }

    /**
     * Tells the linked companion to send its full catalogue ({@code sync}), and every companion
     * that links later too, right after its hello reply. Called once the daemon session is up, so
     * the catalogue is never held in the router's pre-session buffer.
     */
    public void startSync() {
        Peer p;
        synchronized (lock) {
            syncWanted = true;
            p = current;
        }
        if (p != null) {
            p.write(Msg.of("sync")); // outside the lock: a jammed companion blocks only this caller
        }
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
        Peer p;
        Msg m;
        synchronized (lock) {
            prevIme = id == null ? "" : id;
            p = current;
            m = imePrev();
        }
        if (p != null) {
            p.write(m); // outside the lock: a jammed companion blocks only this caller
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
            boolean greeted;
            // p's write lock (Proto.write locks the stream too) is held from before p becomes
            // current until its greeting is written, so no other message can precede the hello
            // reply, ime_prev and sync. The link lock is never held while writing: a companion
            // that stops reading must not block close(), alive() or connect().
            synchronized (p.out) {
                Msg prev;
                boolean sync;
                synchronized (lock) {
                    if (closed || !pending.remove(p)) {
                        p.close(); // closed, or the hello timer already dropped it
                        return;
                    }
                    prev = prevIme != null ? imePrev() : null;
                    sync = syncWanted;
                    old = current;
                    current = p;
                    lock.notifyAll();
                }
                greeted = p.write(Msg.of("hello").put("version", Proto.VERSION).put("token", ""));
                if (greeted && prev != null) {
                    greeted = p.write(prev);
                }
                if (greeted && sync) {
                    greeted = p.write(Msg.of("sync"));
                }
            }
            if (old != null) {
                log.accept("companion: replaced by a new link");
                old.close();
            }
            if (!greeted) {
                return; // the finally unlinks and closes it
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

    /**
     * The catalogue message to relay for {@code m}, or null (logged) if it is malformed, since a
     * malformed one would be a protocol error at lanternd and end the session. {@code app} is
     * rebuilt as {@code pkg, user, label, [work], icon}: {@code work} is the helper's answer for
     * the user, never the companion's. {@code app} and {@code app_removed} update {@link #label}.
     * Logs pkg/user/icon length only, never the icon.
     */
    private Msg relayable(Msg m) {
        try {
            switch (m.t) {
                case "app": {
                    String pkg = m.str("pkg");
                    int user = user(m);
                    String label = m.str("label");
                    String icon = m.str("icon");
                    if (icon.length() > MAX_ICON_B64) {
                        log.accept("companion: app " + pkg + " user=" + user + " icon of " + icon.length()
                            + " base64 chars is over MAX_ICON, sent without it");
                        icon = "";
                    }
                    Msg out = Msg.of("app").put("pkg", pkg).put("user", user).put("label", label);
                    if (isManaged(user)) {
                        out.put("work", Boolean.TRUE);
                    }
                    out.put("icon", icon);
                    labels.put(pkg + ":" + user, label);
                    return out;
                }
                case "app_removed": {
                    String pkg = m.str("pkg");
                    int user = user(m);
                    labels.remove(pkg + ":" + user);
                    return Msg.of("app_removed").put("pkg", pkg).put("user", user);
                }
                case "apps_end": {
                    long count = m.num("count");
                    if (count < 0 || count > 0xffffffffL) {
                        throw new IOException("apps_end.count " + count + " out of range");
                    }
                    log.accept("companion: apps_end count=" + count);
                    return Msg.of("apps_end").put("count", count);
                }
                default:
                    return null;
            }
        } catch (IOException e) {
            Object pkg = m.fields().get("pkg");
            log.accept("companion: malformed " + m.t + (pkg instanceof String ? " pkg=" + pkg : "")
                + " dropped: " + e.getMessage());
            return null;
        }
    }

    private static int user(Msg m) throws IOException {
        long u = m.num("user");
        if (u < 0 || u > Integer.MAX_VALUE) {
            throw new IOException(m.t + ".user " + u + " out of range");
        }
        return (int) u;
    }

    private boolean isManaged(int user) {
        return managed.computeIfAbsent(user, u -> {
            try {
                return managedProfile.test(u);
            } catch (RuntimeException e) {
                log.accept("companion: managed-profile check for user " + u + " failed: " + e);
                return false;
            }
        });
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
                    // Never the parser's message: it may quote the payload.
                    log.accept("companion: undecodable message of " + f.payload.length + " bytes dropped");
                    continue;
                }
                if (RELAYED.contains(m.t)) {
                    Msg out = relayable(m);
                    if (out == null) {
                        continue;
                    }
                    try {
                        toDaemon.accept(out);
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
