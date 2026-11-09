package dev.lantern.helper;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;

/**
 * Host-JVM tests for {@link CompanionLink} over the real {@link HandoffListener} (as on the
 * device). Each fake companion is one end of a loopback socket pair whose other end is handed to
 * the listener, standing in for the device's pipe pair; a fake {@link CompanionLink.Broadcaster}
 * plays the provider call. Android classes must never be loaded here.
 */
public final class CompanionLinkTest {
    static final String TOKEN = "fedcba9876543210fedcba9876543210";
    static final TestCodec CODEC = new TestCodec();

    static CompanionLink.Conn conn(Socket s) {
        return new CompanionLink.Conn() {
            @Override
            public InputStream in() throws IOException {
                return s.getInputStream();
            }

            @Override
            public OutputStream out() throws IOException {
                return s.getOutputStream();
            }

            @Override
            public void close() throws IOException {
                s.close();
            }
        };
    }

    /** One fake companion connection. */
    static final class Companion implements AutoCloseable {
        final Socket s;
        final DataInputStream in;
        final DataOutputStream out;

        /** Hands the helper's end of a fresh connection to {@code l}. */
        Companion(HandoffListener l) throws IOException {
            try (ServerSocket ss = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
                s = new Socket(InetAddress.getLoopbackAddress(), ss.getLocalPort());
                l.offer(conn(ss.accept()));
            }
            s.setSoTimeout(3000);
            in = new DataInputStream(s.getInputStream());
            out = new DataOutputStream(s.getOutputStream());
        }

        void send(String json) throws IOException {
            Proto.write(out, Proto.KIND_CONTROL, 0, json.getBytes(StandardCharsets.UTF_8));
        }

        void hello(String token) throws IOException {
            send("{\"t\":\"hello\",\"version\":1,\"token\":\"" + token + "\"}");
        }

        /** Next control message on stream 0, or null at EOF. */
        Msg read() throws IOException {
            Proto.Frame f;
            try {
                f = Proto.read(in);
            } catch (java.net.SocketException e) {
                return null; // reset by the closing side
            }
            if (f == null) {
                return null;
            }
            eq(Proto.KIND_CONTROL, f.kind, "kind");
            eq(0, f.stream, "stream");
            return CODEC.decode(f.payload);
        }

        void expectHelloReply() throws IOException {
            Msg m = read();
            check(m != null, "hello reply, got EOF");
            eq("hello", m.t, "reply t");
            eq(1L, m.num("version"), "reply version");
            eq("", m.str("token"), "reply token");
            eq("[version, token]", m.fields().keySet().toString(), "reply fields (no prev_ime)");
        }

        @Override
        public void close() throws IOException {
            s.close();
        }
    }

    static final class Rig implements AutoCloseable {
        final HandoffListener listener;
        final List<String> broadcasts = Collections.synchronizedList(new ArrayList<>());
        final BlockingQueue<Msg> toDaemon = new LinkedBlockingQueue<>();
        final List<String> logs = Collections.synchronizedList(new ArrayList<>());
        final CompanionLink link;
        volatile Runnable onBroadcast = () -> { };

        Rig() throws IOException {
            this(u -> false);
        }

        /** {@code managed}: which Android users are managed (work) profiles. */
        Rig(java.util.function.IntPredicate managed) throws IOException {
            listener = new HandoffListener();
            link = new CompanionLink(listener, (socket, token) -> {
                broadcasts.add(socket + " " + token);
                onBroadcast.run();
            }, CODEC, TOKEN, toDaemon::add, logs::add, managed);
        }

        @Override
        public void close() {
            link.close();
        }
    }

    static void wrongTokenClosedRightTokenLinks() throws Exception {
        try (Rig r = new Rig()) {
            List<Companion> companions = Collections.synchronizedList(new ArrayList<>());
            Throwable[] failure = {null};
            r.onBroadcast = () -> {
                Thread t = new Thread(() -> {
                    try {
                        Companion bad = new Companion(r.listener);
                        companions.add(bad);
                        bad.hello("00000000000000000000000000000000");
                        check(bad.read() == null, "bad token: connection closed without a reply");
                        Companion wrongFirst = new Companion(r.listener);
                        companions.add(wrongFirst);
                        wrongFirst.send("{\"t\":\"app\",\"pkg\":\"x\"}");
                        check(wrongFirst.read() == null, "non-hello first frame: closed");
                        Companion good = new Companion(r.listener);
                        companions.add(good);
                        good.hello(TOKEN);
                        good.expectHelloReply();
                    } catch (Throwable e) {
                        failure[0] = e;
                    }
                }, "fake-companion");
                t.setDaemon(true);
                t.start();
            };
            check(r.link.connect(3000), "connect links the authenticated companion");
            Thread.sleep(100);
            if (failure[0] != null) {
                throw new AssertionError("fake companion: " + failure[0], failure[0]);
            }
            eq(1, r.broadcasts.size(), "one broadcast");
            eq(r.listener.name() + " " + TOKEN, r.broadcasts.get(0), "broadcast carries the listener and link token");
            check(r.link.alive(), "alive after linking");
            for (Companion c : companions) {
                c.close();
            }
        }
    }

    static void connectTimesOutWithoutCompanion() throws Exception {
        try (Rig r = new Rig()) {
            long t0 = System.nanoTime();
            check(!r.link.connect(300), "no companion: connect is false");
            long ms = (System.nanoTime() - t0) / 1_000_000;
            check(ms >= 280 && ms <= 500, "returned after the timeout, not much later: " + ms + " ms");
            check(!r.link.alive(), "not alive");
            check(!r.link.send(Msg.of("ime").put("op", "commit").put("text", "x")), "send without a link is false");
        }
    }

    static Companion linked(Rig r) throws Exception {
        Companion[] c = {null};
        r.onBroadcast = () -> {
            try {
                c[0] = new Companion(r.listener);
                c[0].hello(TOKEN);
            } catch (IOException e) {
                throw new AssertionError(e);
            }
        };
        check(r.link.connect(3000), "linked");
        c[0].expectHelloReply();
        return c[0];
    }

    static void relaysCatalogueDropsOthers() throws Exception {
        try (Rig r = new Rig(); Companion c = linked(r)) {
            c.send("{\"t\":\"app\",\"pkg\":\"org.example\",\"user\":0,\"label\":\"Example\",\"icon\":\"\"}");
            c.send("{\"t\":\"clip\",\"text\":\"secret\"}");
            c.send("{\"t\":\"app_removed\",\"pkg\":\"org.gone\",\"user\":0}");
            c.send("{\"t\":\"apps_end\",\"count\":1}");
            Msg a = r.toDaemon.poll(2, TimeUnit.SECONDS);
            check(a != null, "app relayed");
            eq("app", a.t, "first relayed");
            eq("org.example", a.str("pkg"), "app pkg");
            Msg b = r.toDaemon.poll(2, TimeUnit.SECONDS);
            eq("app_removed", b == null ? null : b.t, "second relayed");
            Msg e = r.toDaemon.poll(2, TimeUnit.SECONDS);
            eq("apps_end", e == null ? null : e.t, "third relayed");
            check(r.toDaemon.poll(100, TimeUnit.MILLISECONDS) == null, "clip never relayed");
            boolean logged = false;
            synchronized (r.logs) {
                for (String l : r.logs) {
                    logged |= l.contains("clip") && l.contains("dropped") && !l.contains("secret");
                }
            }
            check(logged, "clip logged as dropped (without its text): " + r.logs);
            check(r.link.alive(), "still linked");
        }
    }

    static void setPrevImeSendsImePrev() throws Exception {
        try (Rig r = new Rig(); Companion a = linked(r)) {
            r.link.setPrevIme("com.example/.Keyboard");
            Msg m = a.read();
            check(m != null, "ime_prev, got EOF");
            eq("ime_prev", m.t, "t");
            eq("com.example/.Keyboard", m.str("id"), "id");
            // A newly linked companion replaces the old one and gets the stored id after its hello.
            try (Companion b = new Companion(r.listener)) {
                b.hello(TOKEN);
                b.expectHelloReply();
                Msg again = b.read();
                check(again != null, "ime_prev re-sent, got EOF");
                eq("ime_prev", again.t, "re-sent t");
                eq("com.example/.Keyboard", again.str("id"), "re-sent id");
                check(a.read() == null, "the replaced companion is closed");
                check(r.link.alive(), "alive with the new companion");
                check(r.link.send(Msg.of("ime").put("op", "commit").put("text", "hi")), "send to the new companion");
                Msg ime = b.read();
                eq("ime", ime == null ? null : ime.t, "ime reaches the new companion");
            }
        }
    }

    static void imePrevBeforeLinkIsSentOnLink() throws Exception {
        try (Rig r = new Rig()) {
            r.link.setPrevIme("");
            try (Companion c = linked(r)) {
                Msg m = c.read();
                check(m != null, "ime_prev, got EOF");
                eq("ime_prev", m.t, "t");
                eq("", m.str("id"), "empty id");
            }
        }
    }

    static void annotatesWorkProfile() throws Exception {
        try (Rig r = new Rig(u -> u == 10); Companion c = linked(r)) {
            c.send("{\"t\":\"app\",\"pkg\":\"com.android.files\",\"user\":10,\"label\":\"Files\",\"icon\":\"iVBO\"}");
            c.send("{\"t\":\"app\",\"pkg\":\"com.android.files\",\"user\":0,\"label\":\"Files\",\"icon\":\"\"}");
            // The companion has no say in work: the helper decides from the user.
            c.send("{\"t\":\"app\",\"pkg\":\"org.liar\",\"user\":0,\"label\":\"Liar\",\"work\":true,\"icon\":\"\"}");
            Msg w = r.toDaemon.poll(2, TimeUnit.SECONDS);
            check(w != null, "work app relayed");
            eq(Boolean.TRUE, w.fields().get("work"), "user 10 is work: " + w.fields().keySet());
            eq("[pkg, user, label, work, icon]", w.fields().keySet().toString(), "field order");
            eq("iVBO", w.str("icon"), "icon kept");
            Msg p = r.toDaemon.poll(2, TimeUnit.SECONDS);
            check(p != null, "personal app relayed");
            eq("[pkg, user, label, icon]", p.fields().keySet().toString(), "user 0: no work key");
            Msg l = r.toDaemon.poll(2, TimeUnit.SECONDS);
            check(l != null, "liar relayed");
            check(!l.fields().containsKey("work"), "companion-sent work dropped: " + l.fields().keySet());
        }
    }

    static void malformedAppDropped() throws Exception {
        try (Rig r = new Rig(); Companion c = linked(r)) {
            c.send("{\"t\":\"app\",\"pkg\":\"x\"}");
            c.send("{\"t\":\"app\",\"pkg\":\"x\",\"user\":-1,\"label\":\"X\",\"icon\":\"\"}");
            c.send("{\"t\":\"app_removed\",\"pkg\":\"x\"}");
            c.send("{\"t\":\"apps_end\",\"count\":-3}");
            c.send("{\"t\":\"apps_end\",\"count\":0}");
            Msg m = r.toDaemon.poll(2, TimeUnit.SECONDS);
            eq("apps_end", m == null ? null : m.t, "only the well-formed message is relayed");
            eq(0L, m.num("count"), "count");
            check(r.toDaemon.poll(100, TimeUnit.MILLISECONDS) == null, "nothing else");
            check(r.link.alive(), "a malformed message does not drop the link");
        }
    }

    static void labelsTrackAppAndRemoved() throws Exception {
        try (Rig r = new Rig(); Companion c = linked(r)) {
            eq(null, r.link.label("org.example", 0), "unknown before");
            c.send("{\"t\":\"app\",\"pkg\":\"org.example\",\"user\":0,\"label\":\"Example\",\"icon\":\"\"}");
            c.send("{\"t\":\"app\",\"pkg\":\"org.example\",\"user\":10,\"label\":\"Example (work)\",\"icon\":\"\"}");
            r.toDaemon.poll(2, TimeUnit.SECONDS);
            r.toDaemon.poll(2, TimeUnit.SECONDS);
            eq("Example", r.link.label("org.example", 0), "label user 0");
            eq("Example (work)", r.link.label("org.example", 10), "label user 10");
            c.send("{\"t\":\"app\",\"pkg\":\"org.example\",\"user\":0,\"label\":\"Renamed\",\"icon\":\"\"}");
            r.toDaemon.poll(2, TimeUnit.SECONDS);
            eq("Renamed", r.link.label("org.example", 0), "an app update replaces the label");
            c.send("{\"t\":\"app_removed\",\"pkg\":\"org.example\",\"user\":0}");
            r.toDaemon.poll(2, TimeUnit.SECONDS);
            eq(null, r.link.label("org.example", 0), "removed");
            eq("Example (work)", r.link.label("org.example", 10), "the other user's entry stays");
        }
    }

    static void syncSentOnSessionUpAndToLateCompanion() throws Exception {
        try (Rig r = new Rig(); Companion a = linked(r)) {
            // Before the session is up: no sync (a marker message arrives first and alone).
            check(r.link.send(Msg.of("ime").put("op", "finish").put("text", "")), "marker");
            eq("ime", a.read().t, "nothing before the marker");
            r.link.startSync();
            Msg s = a.read();
            check(s != null, "sync, got EOF");
            eq("sync", s.t, "sync on session up");
            eq("[]", s.fields().keySet().toString(), "sync has no fields");
            try (Companion b = new Companion(r.listener)) {
                b.hello(TOKEN);
                b.expectHelloReply();
                Msg again = b.read();
                eq("sync", again == null ? null : again.t, "a late companion gets sync after its hello reply");
            }
        }
        try (Rig r = new Rig()) {
            r.link.setPrevIme("k");
            r.link.startSync(); // before any companion links
            try (Companion c = linked(r)) {
                eq("ime_prev", c.read().t, "ime_prev first");
                eq("sync", c.read().t, "then sync");
            }
        }
    }

    static void linkLossIsNotAlive() throws Exception {
        try (Rig r = new Rig()) {
            Companion c = linked(r);
            c.close();
            long end = System.currentTimeMillis() + 2000;
            while (r.link.alive() && System.currentTimeMillis() < end) {
                Thread.sleep(10);
            }
            check(!r.link.alive(), "not alive after the companion disconnects");
        }
    }

    /**
     * A companion that stops reading jams writes to it, but only for the writers: alive(),
     * setPrevIme/startSync callers that queue behind the jam, and close() stay bounded, and close
     * releases the jammed writers.
     */
    static void undrainedPeerDoesNotBlockClose() throws Exception {
        Rig r = new Rig();
        Companion c = linked(r); // never reads again
        String big = new String(new char[Proto.MAX_IME_TEXT]).replace('\0', 'x');
        Thread flood = new Thread(() -> {
            while (r.link.send(Msg.of("ime").put("op", "commit").put("text", big))) {
                // until the write fails (closed)
            }
        }, "fake-ime-flood");
        flood.setDaemon(true);
        flood.start();
        Thread.sleep(500); // socket buffers full: flood is blocked in a write
        check(flood.isAlive(), "the flood is jammed");
        Thread prev = new Thread(() -> r.link.setPrevIme("com.example/.Keyboard"), "fake-prev");
        prev.setDaemon(true);
        prev.start();
        Thread sync = new Thread(r.link::startSync, "fake-sync");
        sync.setDaemon(true);
        sync.start();
        Thread.sleep(200); // both now wait on the jammed peer
        long t0 = System.nanoTime();
        Thread probe = new Thread(() -> {
            r.link.alive();
            r.link.close();
        }, "fake-close");
        probe.setDaemon(true);
        probe.start();
        probe.join(1000);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        check(!probe.isAlive(), "alive() and close() returned within 1 s despite the jam (" + ms + " ms)");
        flood.join(2000);
        prev.join(2000);
        sync.join(2000);
        check(!flood.isAlive() && !prev.isAlive() && !sync.isAlive(), "close released the jammed writers");
        check(!r.link.alive(), "not alive after close");
        c.close();
    }

    static void handoffListenerQueuesAndCloses() throws Exception {
        HandoffListener l = new HandoffListener();
        List<String> closed = Collections.synchronizedList(new ArrayList<>());
        CompanionLink.Conn a = named("a", closed);
        CompanionLink.Conn b = named("b", closed);
        l.offer(a);
        l.offer(b);
        check(l.accept() == a, "first offered is accepted first");
        check(l.accept() == b, "then the second");
        Throwable[] err = {null};
        Thread t = new Thread(() -> {
            try {
                l.accept();
            } catch (Throwable e) {
                err[0] = e;
            }
        });
        t.start();
        Thread.sleep(100);
        check(t.isAlive(), "accept blocks while nothing is offered");
        l.close();
        t.join(1000);
        check(!t.isAlive(), "close unblocks accept");
        check(err[0] instanceof IOException, "accept after close throws IOException: " + err[0]);
        l.offer(named("c", closed));
        eq("[c]", closed.toString(), "an offer after close is closed at once");
        l.close(); // idempotent
    }

    static CompanionLink.Conn named(String name, List<String> closed) {
        return new CompanionLink.Conn() {
            @Override
            public InputStream in() {
                return null;
            }

            @Override
            public OutputStream out() {
                return null;
            }

            @Override
            public void close() {
                closed.add(name);
            }
        };
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
            {"wrongTokenClosedRightTokenLinks", (Case) CompanionLinkTest::wrongTokenClosedRightTokenLinks},
            {"connectTimesOutWithoutCompanion", (Case) CompanionLinkTest::connectTimesOutWithoutCompanion},
            {"relaysCatalogueDropsOthers", (Case) CompanionLinkTest::relaysCatalogueDropsOthers},
            {"setPrevImeSendsImePrev", (Case) CompanionLinkTest::setPrevImeSendsImePrev},
            {"imePrevBeforeLinkIsSentOnLink", (Case) CompanionLinkTest::imePrevBeforeLinkIsSentOnLink},
            {"annotatesWorkProfile", (Case) CompanionLinkTest::annotatesWorkProfile},
            {"malformedAppDropped", (Case) CompanionLinkTest::malformedAppDropped},
            {"labelsTrackAppAndRemoved", (Case) CompanionLinkTest::labelsTrackAppAndRemoved},
            {"syncSentOnSessionUpAndToLateCompanion", (Case) CompanionLinkTest::syncSentOnSessionUpAndToLateCompanion},
            {"linkLossIsNotAlive", (Case) CompanionLinkTest::linkLossIsNotAlive},
            {"undrainedPeerDoesNotBlockClose", (Case) CompanionLinkTest::undrainedPeerDoesNotBlockClose},
            {"handoffListenerQueuesAndCloses", (Case) CompanionLinkTest::handoffListenerQueuesAndCloses},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   CompanionLinkTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL CompanionLinkTest." + c[0] + ": " + t);
            }
        }
        System.out.println("CompanionLinkTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
