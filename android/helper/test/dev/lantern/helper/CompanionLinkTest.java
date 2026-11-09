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
            listener = new HandoffListener();
            link = new CompanionLink(listener, (socket, token) -> {
                broadcasts.add(socket + " " + token);
                onBroadcast.run();
            }, CODEC, TOKEN, toDaemon::add, logs::add);
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
            c.send("{\"t\":\"app\",\"pkg\":\"org.example\",\"user\":0,\"label\":\"Example\"}");
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
            {"linkLossIsNotAlive", (Case) CompanionLinkTest::linkLossIsNotAlive},
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
