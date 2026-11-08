package dev.lantern.helper;

import java.io.Closeable;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.UncheckedIOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Consumer;

/**
 * The helper's server loop for one daemon connection (docs/protocol.md). Requires an authenticated
 * {@code Hello} first, then routes Open/Resize/Focus/Close and Input by window id (the frame's
 * stream) and muxes each window's video back on the same stream.
 *
 * <p>Threads: {@link #run} is the control thread (reads frames, handles Open/Resize/Focus/Close and
 * input). One worker thread polls {@code taskVanished} every {@value #POLL_MS} ms and runs work handed
 * off by display callbacks (stream failures, rotations), so a display is never closed from its own
 * drain thread. Each display's drain thread writes video directly.
 *
 * <p>Per-window failures (open, resize, stream) are reported as {@code error{msg}} then
 * {@code closed{reason:"error"}} on the window's stream. Session failures (bad hello, protocol error)
 * are one {@code error{msg}} on stream 0, after which {@link #run} returns.
 *
 * <p>Teardown never hangs on a jammed socket: it closes the transport first (unblocking any writer),
 * then closes every display (each bounded ~1 s), then the session.
 */
public final class Router {
    static final long POLL_MS = 250;
    /** How long teardown waits for in-flight worker tasks (one display close is ~1 s). */
    static final long WORKER_DRAIN_MS = 3000;
    /** How long a final error write may block before teardown closes the transport under it. */
    static final long FINAL_WRITE_MS = 1000;

    private static final class ProtocolError extends Exception {
        /** Optional machine-readable {@code code} for the final {@code error} (docs/protocol.md). */
        final String code;

        ProtocolError(String msg) {
            this(msg, null);
        }

        ProtocolError(String msg, String code) {
            super(msg);
            this.code = code;
        }
    }

    private final DataInputStream in;
    private final DataOutputStream out;
    private final AppDisplayFactory factory;
    private final Injector injector;
    private final ControlCodec codec;
    private final String token;
    private final Callable<AutoCloseable> sessionStarter;
    private final Closeable transport;
    private final Consumer<String> log;
    private final Map<Integer, Win> windows = new ConcurrentHashMap<>();
    private final ScheduledExecutorService worker = Executors.newSingleThreadScheduledExecutor(r -> {
        Thread t = new Thread(r, "lantern-router");
        t.setDaemon(true);
        return t;
    });
    private AutoCloseable session;
    private volatile boolean stopping;

    /**
     * @param sessionStarter called once, after a valid Hello and before the Hello reply (an
     *     unauthenticated client never changes phone state); its result is closed last at exit
     * @param transport closed first at exit, so writers blocked on {@code out} fail instead of hanging
     */
    public Router(DataInputStream in, DataOutputStream out, AppDisplayFactory factory, Injector injector,
            ControlCodec codec, String token, Callable<AutoCloseable> sessionStarter, Closeable transport,
            Consumer<String> log) {
        this.in = in;
        this.out = out;
        this.factory = factory;
        this.injector = injector;
        this.codec = codec;
        this.token = token;
        this.sessionStarter = sessionStarter;
        this.transport = transport;
        this.log = log;
    }

    /** One open (or opening) window: also its video sink. */
    private final class Win implements VideoSink {
        final int id;
        final AtomicBoolean closing = new AtomicBoolean();
        volatile DisplayHandle handle;
        /** Video produced before Opened went out; flushed right after it. */
        private List<byte[]> pending = new ArrayList<>();
        private boolean ready;

        Win(int id) {
            this.id = id;
        }

        @Override
        public void packet(int win, long ptsUs, boolean config, boolean key, ByteBuffer data) {
            byte[] payload = Proto.encodeVideo(ptsUs, config, key, data);
            synchronized (this) {
                if (closing.get()) {
                    return;
                }
                if (!ready) {
                    pending.add(payload);
                    return;
                }
            }
            try {
                Proto.write(out, Proto.KIND_VIDEO, id, payload);
            } catch (IOException e) {
                // The display's drain loop reports this as a stream failure.
                throw new UncheckedIOException(e);
            }
        }

        /** Sends Opened, then any video produced while opening, then lets video flow live. */
        synchronized void ready(Msg opened) {
            send(id, opened);
            for (byte[] p : pending) {
                try {
                    Proto.write(out, Proto.KIND_VIDEO, id, p);
                } catch (IOException e) {
                    log.accept("win " + id + ": buffered video write failed: " + e);
                    break;
                }
            }
            pending = null;
            ready = true;
        }
    }

    /** Returns at EOF, Bye, a host Error, or after reporting a session/protocol error. */
    public void run() {
        try {
            if (handshake()) {
                worker.scheduleWithFixedDelay(this::poll, POLL_MS, POLL_MS, TimeUnit.MILLISECONDS);
                loop();
            }
        } catch (ProtocolError e) {
            log.accept("router: " + e.getMessage());
            Msg err = Msg.of("error").put("msg", e.getMessage());
            if (e.code != null) {
                // code "version": lanternd fails hard instead of retrying the same helper.
                err.put("code", e.code).put("version", Proto.VERSION);
            }
            sendFinal(err);
        } catch (IOException e) {
            log.accept("router: connection ended: " + e);
        } catch (RuntimeException e) {
            log.accept("router: internal error: " + e);
            sendFinal(Msg.of("error").put("msg", "internal error: " + e));
        } finally {
            teardown();
        }
    }

    private boolean handshake() throws IOException, ProtocolError {
        Proto.Frame f = Proto.read(in);
        if (f == null) {
            log.accept("router: EOF before hello");
            return false;
        }
        if (f.kind != Proto.KIND_CONTROL || f.stream != 0) {
            throw new ProtocolError("expected hello on stream 0 first");
        }
        Msg m = decode(f.payload);
        if (!m.t.equals("hello")) {
            throw new ProtocolError("expected hello first, got " + m.t);
        }
        long version = field(m, "version");
        if (version != Proto.VERSION) {
            throw new ProtocolError("protocol version mismatch: helper speaks " + Proto.VERSION
                + ", host sent " + version + "; update lantern so both sides match", "version");
        }
        String got = text(m, "token");
        if (!MessageDigest.isEqual(got.getBytes(StandardCharsets.UTF_8), token.getBytes(StandardCharsets.UTF_8))) {
            throw new ProtocolError("bad session token: this helper was started by another lanternd");
        }
        try {
            session = sessionStarter.call();
        } catch (Exception e) {
            throw new ProtocolError("session start failed: " + e);
        }
        if (!send(0, Msg.of("hello").put("version", Proto.VERSION).put("token", ""))) {
            throw new IOException("hello reply failed");
        }
        log.accept("router: session up");
        return true;
    }

    private void loop() throws IOException, ProtocolError {
        while (true) {
            Proto.Frame f = Proto.read(in);
            if (f == null) {
                log.accept("router: EOF");
                return;
            }
            if (f.kind == Proto.KIND_CONTROL) {
                if (!control(f.stream, decode(f.payload))) {
                    return;
                }
            } else if (f.kind == Proto.KIND_INPUT) {
                input(f.stream, f.payload);
            } else {
                throw new ProtocolError("unexpected frame kind " + f.kind + " from host");
            }
        }
    }

    /** False when the session should end. */
    private boolean control(int stream, Msg m) throws ProtocolError {
        if (!Msg.TYPES.contains(m.t)) {
            throw new ProtocolError("unknown control type " + m.t);
        }
        if (stream == 0) {
            switch (m.t) {
                case "bye":
                    log.accept("router: bye");
                    return false;
                case "error":
                    log.accept("router: host error: " + m.fields().get("msg"));
                    return false;
                default:
                    throw new ProtocolError(m.t + " is not valid on stream 0");
            }
        }
        switch (m.t) {
            case "open":
                open(stream, m);
                break;
            case "resize":
                resize(stream, m);
                break;
            case "focus":
                focus(stream);
                break;
            case "close":
                close(stream);
                break;
            default:
                throw new ProtocolError(m.t + " is not valid from the host");
        }
        return true;
    }

    private void open(int win, Msg m) throws ProtocolError {
        String pkg = text(m, "pkg");
        int user = range(m, "user", 0, Integer.MAX_VALUE);
        int w = range(m, "w", 1, 16384);
        int h = range(m, "h", 1, 16384);
        int dpi = range(m, "dpi", 1, 4096);
        if (windows.containsKey(win)) {
            send(win, Msg.of("error").put("msg", "window " + win + " is already open"));
            return;
        }
        Win wn = new Win(win);
        DisplayHandle d;
        try {
            d = factory.open(win, pkg, user, w, h, dpi, wn);
        } catch (Exception e) {
            log.accept("router: open win=" + win + " " + pkg + " failed: " + describe(e));
            send(win, Msg.of("error").put("msg", "open " + pkg + " failed: " + describe(e)));
            send(win, Msg.of("closed").put("reason", "error"));
            return;
        }
        wn.handle = d;
        windows.put(win, wn);
        // Opened carries the display's current size: the app may already have rotated it.
        int ow = d.width();
        int oh = d.height();
        Msg opened = Msg.of("opened").put("display", d.displayId()).put("task", d.taskId()).put("codec", d.codec())
            .put("w", ow).put("h", oh);
        boolean moved = d.moved();
        if (moved) {
            // Optional, only when true: a v1 opened without it is byte-identical to before.
            opened.put("moved", Boolean.TRUE);
        }
        wn.ready(opened);
        log.accept("router: opened win=" + win + " " + pkg + " display=" + d.displayId() + " task=" + d.taskId()
            + (moved ? " moved=true (the task existed before the launch; closing the window quits it)" : ""));
        // Listeners after Opened, so nothing they send can precede it. Both hop to the worker: the
        // resize listener runs under the display's lock, the stream listener on its drain thread.
        d.setResizeListener((id, rw, rh) -> submit(() -> {
            if (!wn.closing.get()) {
                send(win, Msg.of("resized").put("w", rw).put("h", rh));
            }
        }));
        d.setStreamListener((id, reason) -> submit(() -> closeWindow(wn, "error", "video stream failed: " + reason)));
        if (d.width() != ow || d.height() != oh) {
            int rw = d.width();
            int rh = d.height();
            submit(() -> send(win, Msg.of("resized").put("w", rw).put("h", rh)));
        }
    }

    private Win live(int win, String what) {
        Win wn = windows.get(win);
        if (wn == null || wn.closing.get()) {
            log.accept("router: " + what + " for unknown or closing win=" + win + ", ignored");
            return null;
        }
        return wn;
    }

    private void resize(int win, Msg m) throws ProtocolError {
        int w = range(m, "w", 1, 16384);
        int h = range(m, "h", 1, 16384);
        int dpi = range(m, "dpi", 1, 4096);
        Win wn = live(win, "resize");
        if (wn == null) {
            return;
        }
        try {
            wn.handle.resize(w, h, dpi);
        } catch (RuntimeException e) {
            closeWindow(wn, "error", "resize to " + w + "x" + h + " failed: " + describe(e));
            return;
        }
        send(win, Msg.of("resized").put("w", wn.handle.width()).put("h", wn.handle.height()));
    }

    private void focus(int win) {
        Win wn = live(win, "focus");
        if (wn == null) {
            return;
        }
        try {
            injector.focus(wn.handle.taskId());
            wn.handle.requestKeyframe();
        } catch (RuntimeException e) {
            log.accept("router: focus win=" + win + " failed: " + e);
        }
    }

    private void close(int win) {
        Win wn = live(win, "close");
        if (wn != null) {
            closeWindow(wn, "closed", null);
        }
    }

    private void input(int win, byte[] payload) throws ProtocolError {
        Proto.InputEvent ev;
        try {
            ev = Proto.decodeInput(payload);
        } catch (IOException e) {
            throw new ProtocolError("bad input event: " + e.getMessage());
        }
        Win wn = windows.get(win);
        if (wn == null || wn.closing.get()) {
            return; // e.g. racing a close; not worth a log line per event
        }
        DisplayHandle d = wn.handle;
        try {
            // The window normalizes to the video it shows, which is the encoder's top-left crop
            // of the display: scale by the video size, not the display's.
            injector.inject(d.displayId(), d.videoWidth(), d.videoHeight(), ev);
        } catch (RuntimeException e) {
            log.accept("router: inject win=" + win + " failed: " + e);
        }
    }

    /**
     * Closes {@code wn} once (later calls, e.g. a stream failure fired by the close itself, are
     * ignored), then reports {@code error{msg}} (if {@code err}) and {@code closed{reason}}. Silent
     * during teardown.
     */
    private void closeWindow(Win wn, String reason, String err) {
        if (!wn.closing.compareAndSet(false, true)) {
            return;
        }
        windows.remove(wn.id, wn);
        log.accept("router: closing win=" + wn.id + " reason=" + reason + (err != null ? " (" + err + ")" : ""));
        try {
            wn.handle.close();
        } catch (RuntimeException e) {
            log.accept("router: close win=" + wn.id + " failed: " + e);
        }
        if (stopping) {
            return;
        }
        if (err != null) {
            send(wn.id, Msg.of("error").put("msg", err));
        }
        send(wn.id, Msg.of("closed").put("reason", reason));
    }

    /** Worker thread, every POLL_MS: windows whose app quit are closed as app_exited. */
    private void poll() {
        for (Win wn : windows.values()) {
            if (stopping) {
                return;
            }
            if (wn.closing.get()) {
                continue;
            }
            try {
                if (wn.handle.taskVanished()) {
                    closeWindow(wn, "app_exited", null);
                }
            } catch (RuntimeException e) {
                log.accept("router: poll win=" + wn.id + " failed: " + e);
            }
        }
    }

    private void submit(Runnable r) {
        try {
            worker.execute(() -> {
                try {
                    r.run();
                } catch (RuntimeException e) {
                    log.accept("router: worker task failed: " + e);
                }
            });
        } catch (RejectedExecutionException e) {
            // tearing down
        }
    }

    private boolean send(int stream, Msg m) {
        try {
            Proto.write(out, Proto.KIND_CONTROL, stream, codec.encode(m));
            return true;
        } catch (IOException e) {
            log.accept("router: send " + m.t + " on " + stream + " failed: " + e);
            return false;
        }
    }

    /** The session-ending error, written by a helper thread so a jammed socket can't stall teardown. */
    private void sendFinal(Msg error) {
        Thread t = new Thread(() -> send(0, error), "lantern-final-error");
        t.setDaemon(true);
        t.start();
        try {
            t.join(FINAL_WRITE_MS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    private void teardown() {
        stopping = true;
        try {
            transport.close();
        } catch (IOException | RuntimeException e) {
            log.accept("router: transport close failed: " + e);
        }
        worker.shutdown();
        try {
            if (!worker.awaitTermination(WORKER_DRAIN_MS, TimeUnit.MILLISECONDS)) {
                log.accept("router: worker still busy after " + WORKER_DRAIN_MS + " ms");
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        for (Win wn : windows.values()) {
            closeWindow(wn, "closed", null);
        }
        if (session != null) {
            try {
                session.close();
            } catch (Exception e) {
                log.accept("router: session close failed: " + e);
            }
        }
        log.accept("router: done");
    }

    private Msg decode(byte[] payload) throws ProtocolError {
        try {
            return codec.decode(payload);
        } catch (IOException e) {
            throw new ProtocolError("bad control message: " + e.getMessage());
        }
    }

    private static long field(Msg m, String key) throws ProtocolError {
        try {
            return m.num(key);
        } catch (IOException e) {
            throw new ProtocolError(e.getMessage());
        }
    }

    private static String text(Msg m, String key) throws ProtocolError {
        try {
            return m.str(key);
        } catch (IOException e) {
            throw new ProtocolError(e.getMessage());
        }
    }

    private static int range(Msg m, String key, int min, int max) throws ProtocolError {
        long v = field(m, key);
        if (v < min || v > max) {
            throw new ProtocolError(m.t + "." + key + "=" + v + " out of range " + min + ".." + max);
        }
        return (int) v;
    }

    /** {@code e} plus its causes' messages: "open x win=1: no launch intent for x". */
    private static String describe(Throwable e) {
        StringBuilder b = new StringBuilder(String.valueOf(e.getMessage() != null ? e.getMessage() : e.toString()));
        for (Throwable c = e.getCause(); c != null && c != c.getCause(); c = c.getCause()) {
            b.append(": ").append(c.getMessage() != null ? c.getMessage() : c.toString());
        }
        return b.toString();
    }
}
