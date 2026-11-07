package dev.lantern.helper;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Host-JVM tests for {@link Router} over a real loopback TCP socket, with fake displays, injector and
 * session. Android classes must never be loaded here.
 */
public final class RouterTest {
    static final String TOKEN = "0123456789abcdef0123456789abcdef";
    static final TestCodec CODEC = new TestCodec();

    static final class FakeHandle implements DisplayHandle {
        final Rig rig;
        final int win;
        final VideoSink sink;
        volatile int w;
        volatile int h;
        volatile boolean vanished;
        volatile boolean failOnClose;
        volatile StreamListener streamListener;
        volatile ResizeListener resizeListener;
        volatile String closeThread;
        volatile int keyframes;

        FakeHandle(Rig rig, int win, int w, int h, VideoSink sink) {
            this.rig = rig;
            this.win = win;
            this.w = w;
            this.h = h;
            this.sink = sink;
        }

        @Override
        public int displayId() {
            return 10 + win;
        }

        @Override
        public int taskId() {
            return 100 + win;
        }

        @Override
        public String codec() {
            return "h264";
        }

        @Override
        public int width() {
            return w;
        }

        @Override
        public int height() {
            return h;
        }

        @Override
        public void resize(int nw, int nh, int dpi) {
            rig.event("resize " + win + " " + nw + "x" + nh + "/" + dpi);
            w = nw;
            h = nh;
        }

        @Override
        public boolean taskVanished() {
            return vanished;
        }

        @Override
        public void requestKeyframe() {
            keyframes++;
        }

        @Override
        public void setStreamListener(StreamListener l) {
            streamListener = l;
        }

        @Override
        public void setResizeListener(ResizeListener l) {
            resizeListener = l;
        }

        @Override
        public void close() {
            if (closeThread != null) {
                return; // idempotent, like AppDisplay
            }
            closeThread = Thread.currentThread().getName();
            if (failOnClose && streamListener != null) {
                // AppDisplay can report a stream failure while a normal close tears the codec down.
                streamListener.streamFailed(win, "codec released during close");
            }
            rig.event("close " + win);
        }
    }

    static final class Rig implements AutoCloseable {
        final List<String> events = Collections.synchronizedList(new ArrayList<>());
        final Map<Integer, FakeHandle> handles = new ConcurrentHashMap<>();
        final Socket client;
        final Socket server;
        final DataOutputStream out;
        final DataInputStream in;
        final Thread thread;
        final List<Proto.Frame> video = Collections.synchronizedList(new ArrayList<>());
        volatile boolean failOnCloseNext;

        Rig() throws IOException {
            try (ServerSocket ss = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
                client = new Socket(InetAddress.getLoopbackAddress(), ss.getLocalPort());
                server = ss.accept();
            }
            client.setSoTimeout(3000);
            out = new DataOutputStream(client.getOutputStream());
            in = new DataInputStream(client.getInputStream());
            AppDisplayFactory factory = (win, pkg, user, w, h, dpi, sink) -> {
                event("open " + win + " " + pkg + " u" + user + " " + w + "x" + h + "/" + dpi);
                if (pkg.equals("bad.pkg")) {
                    throw new IllegalArgumentException("no launch intent for bad.pkg");
                }
                // The encoder's config packet arrives while open is still running.
                sink.packet(win, 0, true, false, ByteBuffer.wrap(new byte[] {0, 0, 0, 1, 0x40}));
                FakeHandle fh = new FakeHandle(this, win, w, h, sink);
                fh.failOnClose = failOnCloseNext;
                handles.put(win, fh);
                return fh;
            };
            Injector injector = new Injector() {
                @Override
                public void inject(int displayId, int w, int h, Proto.InputEvent e) {
                    String what = e instanceof Proto.Pointer ? "pointer" : e instanceof Proto.Key ? "key" : "scroll";
                    event("inject " + what + " display=" + displayId + " " + w + "x" + h);
                }

                @Override
                public void focus(int taskId) {
                    event("focus task=" + taskId);
                }
            };
            Router r = new Router(new DataInputStream(server.getInputStream()),
                new DataOutputStream(server.getOutputStream()), factory, injector, CODEC, TOKEN,
                () -> {
                    event("session start");
                    return () -> event("session close");
                },
                () -> {
                    event("transport close");
                    server.close();
                },
                msg -> { });
            thread = new Thread(r::run, "router");
            thread.start();
        }

        void event(String e) {
            events.add(e);
        }

        void send(int stream, String json) throws IOException {
            Proto.write(out, Proto.KIND_CONTROL, stream, json.getBytes(java.nio.charset.StandardCharsets.UTF_8));
        }

        void hello() throws IOException {
            send(0, "{\"t\":\"hello\",\"version\":1,\"token\":\"" + TOKEN + "\"}");
            Msg m = control(0);
            eq("hello", m.t, "hello reply");
            eq(1L, m.num("version"), "hello version");
            eq("", m.str("token"), "hello token");
        }

        FakeHandle open(int win) throws IOException {
            send(win, "{\"t\":\"open\",\"pkg\":\"com.android.settings\",\"user\":0,\"w\":1280,\"h\":800,\"dpi\":160}");
            Msg m = control(win);
            eq("opened", m.t, "opened");
            return handles.get(win);
        }

        /** Next frame; Video frames are recorded and skipped. */
        Proto.Frame frame() throws IOException {
            while (true) {
                Proto.Frame f = Proto.read(in);
                if (f == null || f.kind != Proto.KIND_VIDEO) {
                    return f;
                }
                video.add(f);
            }
        }

        Msg control(int stream) throws IOException {
            Proto.Frame f = frame();
            check(f != null, "expected a control frame, got EOF");
            eq(Proto.KIND_CONTROL, f.kind, "kind");
            eq(stream, f.stream, "stream of " + new String(f.payload, java.nio.charset.StandardCharsets.UTF_8));
            return CODEC.decode(f.payload);
        }

        void awaitExit(long ms) throws InterruptedException {
            thread.join(ms);
            check(!thread.isAlive(), "router still running after " + ms + " ms");
        }

        /** Polls until {@code cond} holds or {@code ms} passes. */
        static void await(String what, long ms, java.util.function.BooleanSupplier cond) throws InterruptedException {
            long end = System.currentTimeMillis() + ms;
            while (!cond.getAsBoolean()) {
                if (System.currentTimeMillis() > end) {
                    throw new AssertionError("timed out waiting for " + what);
                }
                Thread.sleep(10);
            }
        }

        @Override
        public void close() throws Exception {
            client.close();
            thread.join(5000);
        }
    }

    static void rejectsWrongToken() throws Exception {
        try (Rig r = new Rig()) {
            r.send(0, "{\"t\":\"hello\",\"version\":1,\"token\":\"ffffffffffffffffffffffffffffffff\"}");
            Msg m = r.control(0);
            eq("error", m.t, "reply");
            check(m.str("msg").contains("token"), "msg explains token: " + m.str("msg"));
            r.awaitExit(2000);
            check(r.frame() == null, "EOF after error");
            check(!r.events.contains("session start"), "session must not start: " + r.events);
        }
    }

    static void rejectsWrongVersion() throws Exception {
        try (Rig r = new Rig()) {
            r.send(0, "{\"t\":\"hello\",\"version\":2,\"token\":\"" + TOKEN + "\"}");
            Msg m = r.control(0);
            eq("error", m.t, "reply");
            check(m.str("msg").startsWith("protocol version mismatch: helper speaks 1, host sent 2"),
                "msg explains version: " + m.str("msg"));
            // Structured, so lanternd fails hard (exit 3) instead of retrying (I1).
            eq("version", m.str("code"), "error code");
            eq(1L, m.num("version"), "helper's own version");
            eq("[msg, code, version]", m.fields().keySet().toString(), "field order");
            r.awaitExit(2000);
            check(!r.events.contains("session start"), "session must not start: " + r.events);
        }
    }

    static void requiresHelloFirst() throws Exception {
        try (Rig r = new Rig()) {
            r.send(1, "{\"t\":\"open\",\"pkg\":\"com.android.settings\",\"user\":0,\"w\":1,\"h\":1,\"dpi\":1}");
            eq("error", r.control(0).t, "reply");
            r.awaitExit(2000);
            check(r.handles.isEmpty(), "nothing opened");
        }
    }

    static void helloStartsSessionAndReplies() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            check(r.events.contains("session start"), "session started: " + r.events);
            r.send(0, "{\"t\":\"bye\"}");
            r.awaitExit(2000);
            eq("session close", r.events.get(r.events.size() - 1), "session closed last");
        }
    }

    static void openThenCloseCallsFactoryAndClose() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            r.send(3, "{\"t\":\"open\",\"pkg\":\"com.android.settings\",\"user\":0,\"w\":1280,\"h\":800,\"dpi\":160}");
            // Opened comes before any video for the window, even though the config packet was
            // produced while open() was still running.
            Proto.Frame f = Proto.read(r.in);
            eq(Proto.KIND_CONTROL, f.kind, "first frame after open is control");
            eq(3, f.stream, "opened stream");
            Msg m = CODEC.decode(f.payload);
            eq("opened", m.t, "opened");
            eq(13L, m.num("display"), "display");
            eq(103L, m.num("task"), "task");
            eq("h264", m.str("codec"), "codec");
            eq(1280L, m.num("w"), "w");
            eq(800L, m.num("h"), "h");
            Proto.Frame v = Proto.read(r.in);
            eq(Proto.KIND_VIDEO, v.kind, "buffered config packet follows opened");
            eq(3, v.stream, "video stream");
            eq(Proto.VIDEO_FLAG_CONFIG, v.payload[8] & 0xff, "config flag");
            check(r.events.contains("open 3 com.android.settings u0 1280x800/160"), "factory args: " + r.events);
            FakeHandle h = r.handles.get(3);
            h.sink.packet(3, 1234, false, true, ByteBuffer.wrap(new byte[] {9, 9}));
            r.send(3, "{\"t\":\"close\"}");
            Msg c = r.control(3);
            eq("closed", c.t, "closed");
            eq("closed", c.str("reason"), "reason");
            check(r.events.contains("close 3"), "display closed: " + r.events);
            eq(1, r.video.size(), "live video frame relayed before closed");
            r.send(0, "{\"t\":\"bye\"}");
            r.awaitExit(2000);
        }
    }

    static void eofClosesAllDisplays() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            r.open(1);
            r.open(2);
            r.client.shutdownOutput();
            r.awaitExit(5000);
            List<String> ev = new ArrayList<>(r.events);
            int c1 = ev.indexOf("close 1");
            int c2 = ev.indexOf("close 2");
            int s = ev.indexOf("session close");
            check(c1 >= 0 && c2 >= 0, "both displays closed: " + ev);
            check(s > c1 && s > c2, "session closed after displays: " + ev);
            check(ev.contains("transport close"), "transport closed: " + ev);
        }
    }

    static void appExitSendsClosedAppExited() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            FakeHandle h = r.open(1);
            long t0 = System.currentTimeMillis();
            h.vanished = true;
            Msg m = r.control(1);
            eq("closed", m.t, "closed");
            eq("app_exited", m.str("reason"), "reason");
            check(System.currentTimeMillis() - t0 < 1500, "within a few polls");
            check(r.events.contains("close 1"), "display closed: " + r.events);
            // The window is gone: a later Close for it is ignored, the router keeps running.
            r.send(1, "{\"t\":\"close\"}");
            r.send(0, "{\"t\":\"bye\"}");
            r.awaitExit(2000);
            check(r.frame() == null, "no second closed");
        }
    }

    static void streamFailureClosesWithErrorOffDrainThread() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            FakeHandle h = r.open(1);
            Thread drain = new Thread(() -> h.streamListener.streamFailed(1, "encoder died"), "fake-drain");
            drain.start();
            drain.join(500);
            check(!drain.isAlive(), "listener must not block the drain thread");
            Msg e = r.control(1);
            eq("error", e.t, "error first");
            check(e.str("msg").contains("encoder died"), "error msg: " + e.str("msg"));
            Msg c = r.control(1);
            eq("closed", c.t, "then closed");
            eq("error", c.str("reason"), "reason");
            check(h.closeThread != null && !h.closeThread.equals("fake-drain"),
                "closed off the drain thread: " + h.closeThread);
        }
    }

    static void streamFailureDuringCloseIgnored() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            r.failOnCloseNext = true;
            r.open(1);
            r.send(1, "{\"t\":\"close\"}");
            Msg c = r.control(1);
            eq("closed", c.t, "closed");
            eq("closed", c.str("reason"), "reason");
            r.send(0, "{\"t\":\"bye\"}");
            r.awaitExit(2000);
            Proto.Frame f = r.frame();
            check(f == null, "nothing after closed, got "
                + (f == null ? "" : new String(f.payload, java.nio.charset.StandardCharsets.UTF_8)));
        }
    }

    static void resizeAndRotationSendResized() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            FakeHandle h = r.open(1);
            r.send(1, "{\"t\":\"resize\",\"w\":1000,\"h\":1000,\"dpi\":160}");
            Msg m = r.control(1);
            eq("resized", m.t, "resized");
            eq(1000L, m.num("w"), "w");
            eq(1000L, m.num("h"), "h");
            check(r.events.contains("resize 1 1000x1000/160"), "resize forwarded: " + r.events);
            // The app rotated its display.
            h.resizeListener.resized(1, 800, 1280);
            Msg rot = r.control(1);
            eq("resized", rot.t, "rotation resized");
            eq(800L, rot.num("w"), "rot w");
            eq(1280L, rot.num("h"), "rot h");
        }
    }

    static void inputAndFocusRouteToWindow() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            FakeHandle h = r.open(2);
            ByteBuffer p = ByteBuffer.allocate(1 + 1 + 8 + 4 + 4 + 4 + 4);
            p.put((byte) Proto.INPUT_POINTER).put((byte) Proto.POINTER_DOWN).putLong(1).putFloat(.5f).putFloat(.5f)
                .putInt(0).putFloat(1f);
            Proto.write(r.out, Proto.KIND_INPUT, 2, p.array());
            ByteBuffer k = ByteBuffer.allocate(1 + 1 + 12);
            k.put((byte) Proto.INPUT_KEY).put((byte) Proto.KEY_DOWN).putInt(20).putInt(0).putInt(0);
            Proto.write(r.out, Proto.KIND_INPUT, 2, k.array());
            Proto.write(r.out, Proto.KIND_INPUT, 9, k.array()); // unknown window: ignored
            r.send(2, "{\"t\":\"focus\"}");
            Rig.await("focus", 2000, () -> r.events.contains("focus task=102"));
            List<String> ev = new ArrayList<>(r.events);
            check(ev.contains("inject pointer display=12 1280x800"), "pointer injected: " + ev);
            check(ev.contains("inject key display=12 1280x800"), "key injected: " + ev);
            eq(1, h.keyframes, "focus requests a keyframe");
            check(r.thread.isAlive(), "still serving");
        }
    }

    static void openFailureSendsErrorAndClosed() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            r.send(4, "{\"t\":\"open\",\"pkg\":\"bad.pkg\",\"user\":0,\"w\":1280,\"h\":800,\"dpi\":160}");
            Msg e = r.control(4);
            eq("error", e.t, "error");
            check(e.str("msg").contains("bad.pkg"), "msg: " + e.str("msg"));
            Msg c = r.control(4);
            eq("closed", c.t, "closed");
            eq("error", c.str("reason"), "reason");
            r.open(5); // the session survives
        }
    }

    static void unknownControlTypeIsAnError() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            r.send(0, "{\"t\":\"frobnicate\"}");
            Msg e = r.control(0);
            eq("error", e.t, "error");
            r.awaitExit(2000);
        }
    }

    static void blockedWriterDoesNotHangTeardown() throws Exception {
        try (Rig r = new Rig()) {
            r.hello();
            FakeHandle h = r.open(1);
            // A drain thread floods video while the client reads nothing: its write blocks.
            Thread flood = new Thread(() -> {
                byte[] big = new byte[256 * 1024];
                try {
                    while (h.closeThread == null) {
                        h.sink.packet(1, 0, false, false, ByteBuffer.wrap(big));
                    }
                } catch (RuntimeException e) {
                    // write failed once the transport closed
                }
            }, "fake-drain");
            flood.setDaemon(true);
            flood.start();
            Thread.sleep(500);
            r.client.shutdownOutput(); // EOF for the router while its output is jammed
            r.awaitExit(5000);
            flood.join(2000);
            check(!flood.isAlive(), "blocked writer released");
            check(r.events.contains("session close"), "session closed: " + r.events);
        }
    }

    static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"rejectsWrongToken", (Case) RouterTest::rejectsWrongToken},
            {"rejectsWrongVersion", (Case) RouterTest::rejectsWrongVersion},
            {"requiresHelloFirst", (Case) RouterTest::requiresHelloFirst},
            {"helloStartsSessionAndReplies", (Case) RouterTest::helloStartsSessionAndReplies},
            {"openThenCloseCallsFactoryAndClose", (Case) RouterTest::openThenCloseCallsFactoryAndClose},
            {"eofClosesAllDisplays", (Case) RouterTest::eofClosesAllDisplays},
            {"appExitSendsClosedAppExited", (Case) RouterTest::appExitSendsClosedAppExited},
            {"streamFailureClosesWithErrorOffDrainThread", (Case) RouterTest::streamFailureClosesWithErrorOffDrainThread},
            {"streamFailureDuringCloseIgnored", (Case) RouterTest::streamFailureDuringCloseIgnored},
            {"resizeAndRotationSendResized", (Case) RouterTest::resizeAndRotationSendResized},
            {"inputAndFocusRouteToWindow", (Case) RouterTest::inputAndFocusRouteToWindow},
            {"openFailureSendsErrorAndClosed", (Case) RouterTest::openFailureSendsErrorAndClosed},
            {"unknownControlTypeIsAnError", (Case) RouterTest::unknownControlTypeIsAnError},
            {"blockedWriterDoesNotHangTeardown", (Case) RouterTest::blockedWriterDoesNotHangTeardown},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   RouterTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL RouterTest." + c[0] + ": " + t);
            }
        }
        System.out.println("RouterTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
