package dev.lantern.companion;

import dev.lantern.helper.Msg;
import dev.lantern.helper.Proto;
import dev.lantern.helper.TestCodec;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Host-JVM tests for the companion's side of the link, {@link LinkClient}, against a fake helper
 * over a loopback socket. Android classes must never be loaded here.
 */
public final class LinkClientTest {
    static final String TOKEN = "fedcba9876543210fedcba9876543210";
    static final TestCodec CODEC = new TestCodec();

    /** The fake helper's end plus a client wired to the other end. */
    static final class Rig implements AutoCloseable {
        final Socket helper;
        final Socket companion;
        final DataInputStream in;
        final DataOutputStream out;
        final List<String> logs = Collections.synchronizedList(new ArrayList<>());
        final LinkClient client;

        Rig() throws IOException {
            try (ServerSocket ss = new ServerSocket(0, 1, InetAddress.getLoopbackAddress())) {
                companion = new Socket(InetAddress.getLoopbackAddress(), ss.getLocalPort());
                helper = ss.accept();
            }
            helper.setSoTimeout(3000);
            companion.setSoTimeout(3000);
            in = new DataInputStream(helper.getInputStream());
            out = new DataOutputStream(helper.getOutputStream());
            client = new LinkClient(companion.getInputStream(), companion.getOutputStream(),
                CODEC, logs::add);
        }

        void send(String json) throws IOException {
            Proto.write(out, Proto.KIND_CONTROL, 0, json.getBytes(StandardCharsets.UTF_8));
        }

        Msg read() throws IOException {
            Proto.Frame f = Proto.read(in);
            check(f != null, "frame, got EOF");
            eq(Proto.KIND_CONTROL, f.kind, "kind");
            eq(0, f.stream, "stream");
            return CODEC.decode(f.payload);
        }

        /** Runs the client's handshake on a thread, playing the helper's side with {@code reply}. */
        Throwable handshake(String reply) throws Exception {
            Throwable[] err = {null};
            Thread t = new Thread(() -> {
                try {
                    client.handshake(TOKEN);
                } catch (Throwable e) {
                    err[0] = e;
                }
            });
            t.start();
            Msg hello = read();
            eq("hello", hello.t, "client sends hello first");
            eq(1L, hello.num("version"), "version");
            eq(TOKEN, hello.str("token"), "token");
            if (reply == null) {
                helper.close();
            } else {
                send(reply);
            }
            t.join(3000);
            check(!t.isAlive(), "handshake returned");
            return err[0];
        }

        @Override
        public void close() throws IOException {
            helper.close();
            companion.close();
        }
    }

    static void handshakeSendsHelloAndAcceptsReply() throws Exception {
        try (Rig r = new Rig()) {
            Throwable e = r.handshake("{\"t\":\"hello\",\"version\":1,\"token\":\"\"}");
            check(e == null, "handshake ok: " + e);
        }
    }

    static void handshakeFailsWhenClosed() throws Exception {
        try (Rig r = new Rig()) {
            Throwable e = r.handshake(null);
            check(e instanceof IOException, "EOF instead of a reply is an IOException: " + e);
        }
    }

    static void handshakeRejectsWrongReply() throws Exception {
        try (Rig r = new Rig()) {
            Throwable e = r.handshake("{\"t\":\"hello\",\"version\":2,\"token\":\"\"}");
            check(e instanceof IOException, "version mismatch: " + e);
        }
        try (Rig r = new Rig()) {
            Throwable e = r.handshake("{\"t\":\"ime_prev\",\"id\":\"x\"}");
            check(e instanceof IOException, "not a hello: " + e);
        }
    }

    static void runStoresImePrevRoutesImeDropsOthers() throws Exception {
        try (Rig r = new Rig()) {
            check(r.handshake("{\"t\":\"hello\",\"version\":1,\"token\":\"\"}") == null, "linked");
            List<String> got = Collections.synchronizedList(new ArrayList<>());
            Thread t = new Thread(() -> {
                try {
                    r.client.run(new LinkClient.Handler() {
                        @Override
                        public void imePrev(String id) {
                            got.add("ime_prev " + id);
                        }

                        @Override
                        public void ime(int display, String op, String text) {
                            got.add("ime " + display + " " + op + " " + text);
                        }

                        @Override
                        public void sync() {
                            got.add("sync");
                        }
                    });
                    got.add("eof");
                } catch (IOException e) {
                    got.add("error " + e);
                }
            });
            t.start();
            r.send("{\"t\":\"ime_prev\",\"id\":\"com.example/.Keyboard\"}");
            r.send("{\"t\":\"clip\",\"text\":\"secret\"}");
            r.send("{\"t\":\"ime\",\"op\":\"commit\",\"text\":\"hi\",\"display\":12}");
            r.send("{\"t\":\"ime\",\"op\":\"finish\",\"text\":\"\"}");
            r.send("{\"t\":\"sync\"}");
            r.helper.shutdownOutput();
            t.join(3000);
            check(!t.isAlive(), "run returns at EOF");
            eq("[ime_prev com.example/.Keyboard, ime 12 commit hi, ime -1 finish , sync, eof]", got.toString(),
                "dispatch (display -1 when absent)");
            boolean logged = false;
            synchronized (r.logs) {
                for (String l : r.logs) {
                    logged |= l.contains("clip") && l.contains("dropped") && !l.contains("secret");
                }
            }
            check(logged, "clip logged as dropped without its text: " + r.logs);
        }
    }

    /** An undecodable frame is logged by type of failure and size, never with its payload (M1). */
    static void undecodableFrameNeverLogsThePayload() throws Exception {
        try (Rig r = new Rig()) {
            check(r.handshake("{\"t\":\"hello\",\"version\":1,\"token\":\"\"}") == null, "linked");
            Thread t = new Thread(() -> {
                try {
                    r.client.run(new LinkClient.Handler() {
                        @Override
                        public void imePrev(String id) {
                        }

                        @Override
                        public void ime(int display, String op, String text) {
                        }

                        @Override
                        public void sync() {
                        }
                    });
                } catch (IOException e) {
                    // EOF
                }
            });
            t.start();
            r.send("{\"t\":\"ime\",\"op\":\"commit\",\"text\":\"hunter2 secret\""); // no closing brace
            r.helper.shutdownOutput();
            t.join(3000);
            boolean logged = false;
            synchronized (r.logs) {
                for (String l : r.logs) {
                    check(!l.contains("hunter2"), "log carries the payload: " + l);
                    logged |= l.contains("undecodable") && l.contains("bytes");
                }
            }
            check(logged, "dropped frame logged with its size: " + r.logs);
        }
    }

    static void sendWritesControlFrames() throws Exception {
        try (Rig r = new Rig()) {
            check(r.client.send(Msg.of("apps_end").put("count", 0)), "send ok");
            Msg m = r.read();
            eq("apps_end", m.t, "t");
            eq(0L, m.num("count"), "count");
        }
    }

    static void linkTypesAddImePrevAndSyncOnly() {
        for (String t : new String[] {"ime_prev", "sync"}) {
            check(LinkClient.LINK_TYPES.contains(t), t + " is a link type");
            check(!Msg.TYPES.contains(t), t + " is never a daemon type");
        }
        check(LinkClient.LINK_TYPES.containsAll(Msg.TYPES), "link types include every v1 type");
        eq(Msg.TYPES.size() + 2, LinkClient.LINK_TYPES.size(), "exactly two link-only types");
    }

    static void validatesLinkCall() {
        check(LinkClient.callerAllowed(2000), "shell may link");
        check(LinkClient.callerAllowed(0), "root may link");
        check(!LinkClient.callerAllowed(10178), "an app may not");
        check(!LinkClient.callerAllowed(1000), "system_server may not either");
        check(LinkClient.validToken(TOKEN), "token");
        check(!LinkClient.validToken(TOKEN.toUpperCase()), "upper-case token");
        check(!LinkClient.validToken(TOKEN.substring(1)), "short token");
        check(!LinkClient.validToken(null), "null token");
    }

    static void testActivityStateFromExtra() {
        eq(Boolean.TRUE, LinkClient.testActivityState("true"), "enable");
        eq(Boolean.FALSE, LinkClient.testActivityState("false"), "disable");
        eq(null, LinkClient.testActivityState(null), "missing");
        eq(null, LinkClient.testActivityState("yes"), "garbage");
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
            {"handshakeSendsHelloAndAcceptsReply", (Case) LinkClientTest::handshakeSendsHelloAndAcceptsReply},
            {"handshakeFailsWhenClosed", (Case) LinkClientTest::handshakeFailsWhenClosed},
            {"handshakeRejectsWrongReply", (Case) LinkClientTest::handshakeRejectsWrongReply},
            {"runStoresImePrevRoutesImeDropsOthers", (Case) LinkClientTest::runStoresImePrevRoutesImeDropsOthers},
            {"undecodableFrameNeverLogsThePayload", (Case) LinkClientTest::undecodableFrameNeverLogsThePayload},
            {"sendWritesControlFrames", (Case) LinkClientTest::sendWritesControlFrames},
            {"linkTypesAddImePrevAndSyncOnly", (Case) LinkClientTest::linkTypesAddImePrevAndSyncOnly},
            {"validatesLinkCall", (Case) LinkClientTest::validatesLinkCall},
            {"testActivityStateFromExtra", (Case) LinkClientTest::testActivityStateFromExtra},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   LinkClientTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL LinkClientTest." + c[0] + ": " + t);
            }
        }
        System.out.println("LinkClientTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
