package dev.lantern.helper;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Arrays;

/**
 * Host-JVM tests for {@link Proto}. Uses only java.*; Android classes must never be loaded here.
 * Golden vectors are produced by the Rust side (host/proto/tests/golden.rs).
 */
public final class ProtoTest {
    private static Path golden(String name) {
        String dir = System.getProperty("lantern.golden", "host/proto/golden");
        return Paths.get(dir, name + ".bin");
    }

    private static Proto.Frame readOnly(String name) throws IOException {
        byte[] bytes = Files.readAllBytes(golden(name));
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        Proto.Frame f = Proto.read(in);
        check(f != null, name + ": expected a frame");
        check(Proto.read(in) == null, name + ": expected clean EOF after one frame");
        return f;
    }

    static void goldenControlOpenMatchesRust() throws IOException {
        Proto.Frame f = readOnly("control_open");
        eq(Proto.KIND_CONTROL, f.kind, "kind");
        eq(0, f.stream, "stream");
        // org.json is not on the host JVM, so check the exact JSON text the Rust side produced.
        String json = new String(f.payload, StandardCharsets.UTF_8);
        eq("{\"t\":\"open\",\"pkg\":\"com.android.settings\",\"user\":0,\"w\":1280,\"h\":800,\"dpi\":160}", json, "json");
        check(json.contains("\"t\":\"open\""), "t");
        check(json.contains("\"pkg\":\"com.android.settings\""), "pkg");
        check(json.contains("\"w\":1280"), "w");
    }

    /** The optional host-only activation_token must not break the helper's Open parsing. */
    static void goldenControlOpenWithTokenParses() throws Exception {
        Proto.Frame f = readOnly("control_open_token");
        eq(Proto.KIND_CONTROL, f.kind, "kind");
        eq(0, f.stream, "stream");
        Msg m = new TestCodec().decode(f.payload);
        eq("open", m.t, "t");
        eq("com.android.settings", m.str("pkg"), "pkg");
        eq(0L, m.num("user"), "user");
        eq(1280L, m.num("w"), "w");
        eq(800L, m.num("h"), "h");
        eq(160L, m.num("dpi"), "dpi");
        eq("lantern-xdg-token-1", m.str("activation_token"), "activation_token");
    }

    /** {"t":"focus"} stays the v1 bytes now that Focus carries an optional activation_token. */
    static void goldenControlFocusMatchesRust() throws Exception {
        Proto.Frame f = readOnly("control_focus");
        eq(Proto.KIND_CONTROL, f.kind, "kind");
        eq(1, f.stream, "stream");
        eq("{\"t\":\"focus\"}", new String(f.payload, StandardCharsets.UTF_8), "json");
        Msg m = new TestCodec().decode(f.payload);
        eq("focus", m.t, "t");
        check(m.fields().isEmpty(), "no fields");
    }

    static void goldenInputPointerMatchesRust() throws IOException {
        Proto.Frame f = readOnly("input_pointer");
        eq(Proto.KIND_INPUT, f.kind, "kind");
        eq(1, f.stream, "stream");
        Proto.InputEvent ev = Proto.decodeInput(f.payload);
        check(ev instanceof Proto.Pointer, "pointer type");
        Proto.Pointer p = (Proto.Pointer) ev;
        eq(Proto.POINTER_MOVE, p.action, "action");
        eq(7L, p.id, "id");
        eq(0.25f, p.x, "x");
        eq(0.75f, p.y, "y");
        eq(1, p.buttons, "buttons");
        eq(0.5f, p.pressure, "pressure");
    }

    static void goldenInputKeyMatchesRust() throws IOException {
        Proto.Frame f = readOnly("input_key");
        eq(Proto.KIND_INPUT, f.kind, "kind");
        eq(1, f.stream, "stream");
        Proto.Key k = (Proto.Key) Proto.decodeInput(f.payload);
        eq(Proto.KEY_DOWN, k.action, "action");
        eq(29, k.keycode, "keycode");
        eq(1, k.meta, "meta");
        eq(0, k.repeat, "repeat");
    }

    static void goldenVideoKeyMatchesRust() throws IOException {
        Proto.Frame f = readOnly("video_key");
        eq(Proto.KIND_VIDEO, f.kind, "kind");
        eq(1, f.stream, "stream");
        byte[] want = Proto.encodeVideo(1_000_000L, false, true, new byte[] {0, 0, 0, 1, 0x65, (byte) 0x88, (byte) 0x84, 0});
        check(Arrays.equals(want, f.payload), "video payload bytes");
        eq(0x02, f.payload[8] & 0xff, "flags byte");
        // And the whole frame re-encodes byte-identically.
        check(Arrays.equals(Files.readAllBytes(golden("video_key")), frameBytes(f.kind, f.stream, f.payload)), "frame bytes");
    }

    static void writeThenReadRoundTrip() throws IOException {
        int[][] hdr = {{Proto.KIND_CONTROL, 0}, {Proto.KIND_VIDEO, 7}, {Proto.KIND_INPUT, 0xdeadbeef}};
        byte[][] payloads = {"{\"t\":\"bye\"}".getBytes(StandardCharsets.UTF_8), {1, 2, 3, 4, 5, 6, 7, 8, 9, 10}, {}};
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        for (int i = 0; i < hdr.length; i++) {
            Proto.write(out, hdr[i][0], hdr[i][1], payloads[i]);
        }
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(buf.toByteArray()));
        for (int i = 0; i < hdr.length; i++) {
            Proto.Frame f = Proto.read(in);
            eq(hdr[i][0], f.kind, "kind " + i);
            eq(hdr[i][1], f.stream, "stream " + i);
            check(Arrays.equals(payloads[i], f.payload), "payload " + i);
        }
        check(Proto.read(in) == null, "clean EOF at end");
        // Header layout matches the Rust frame_header_layout test.
        byte[] b = frameBytes(Proto.KIND_VIDEO, 0x01020304, new byte[] {(byte) 0xaa, (byte) 0xbb});
        check(Arrays.equals(new byte[] {0, 0, 0, 7, 2, 1, 2, 3, 4, (byte) 0xaa, (byte) 0xbb}, b), "header layout");
    }

    static void cleanEofReturnsNull() throws IOException {
        check(Proto.read(new DataInputStream(new ByteArrayInputStream(new byte[0]))) == null, "empty stream");
    }

    static void midFrameEofThrows() throws IOException {
        expectEof(new byte[] {0, 0}, "inside len");
        expectEof(new byte[] {0, 0, 0, 10, 1, 0, 0}, "inside header");
        expectEof(new byte[] {0, 0, 0, 10, 1, 0, 0, 0, 0, 1}, "inside payload");
    }

    static void invalidFramesRejected() throws IOException {
        expectInvalid(new byte[] {0, 0, 0, 4, 1, 0, 0, 0}, "short len");
        expectInvalid(new byte[] {0, 0, 0, 5, 9, 0, 0, 0, 0}, "bad kind");
        int over = Proto.MAX_PAYLOAD + 6;
        expectInvalid(new byte[] {(byte) (over >>> 24), (byte) (over >>> 16), (byte) (over >>> 8), (byte) over, 1, 0, 0, 0, 0},
                "oversize");
        DataOutputStream sink = new DataOutputStream(new ByteArrayOutputStream());
        boolean threw = false;
        try {
            Proto.write(sink, Proto.KIND_VIDEO, 1, new byte[Proto.MAX_PAYLOAD + 1]);
        } catch (IOException e) {
            threw = true;
        }
        check(threw, "oversize write rejected");
    }

    static void inputDecodeMirrorsRust() throws IOException {
        // Layout from Rust input_layout_and_errors.
        Proto.Key k = (Proto.Key) Proto.decodeInput(new byte[] {3, 0, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 0});
        eq(29, k.keycode, "keycode");
        Proto.Scroll s = (Proto.Scroll) Proto.decodeInput(new byte[] {
                2, 0x3f, 0, 0, 0, 0x3e, 0, 0, 0, (byte) 0xbf, (byte) 0x80, 0, 0, 0x40, 0x20, 0, 0});
        eq(0.5f, s.x, "scroll x");
        eq(0.125f, s.y, "scroll y");
        eq(-1.0f, s.h, "scroll h");
        eq(2.5f, s.v, "scroll v");
        expectBadInput(new byte[] {}, "empty");
        expectBadInput(new byte[] {9}, "unknown type");
        expectBadInput(new byte[] {3, 0, 0}, "short key");
        expectBadInput(new byte[] {3, 2, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 0}, "bad key action");
        expectBadInput(new byte[26], "type 0");
        byte[] badPointerAction = new byte[26];
        badPointerAction[0] = 1;
        badPointerAction[1] = 5;
        expectBadInput(badPointerAction, "bad pointer action");
        expectBadInput(new byte[] {3, 0, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 0, 0}, "trailing byte");
    }

    // ---------------------------------------------------------------------------------------------

    private static byte[] frameBytes(int kind, int stream, byte[] payload) throws IOException {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        Proto.write(new DataOutputStream(buf), kind, stream, payload);
        return buf.toByteArray();
    }

    private static void expectEof(byte[] bytes, String what) throws IOException {
        try {
            Proto.read(new DataInputStream(new ByteArrayInputStream(bytes)));
        } catch (EOFException e) {
            return;
        }
        throw new AssertionError(what + ": expected EOFException");
    }

    private static void expectInvalid(byte[] bytes, String what) {
        try {
            Proto.read(new DataInputStream(new ByteArrayInputStream(bytes)));
        } catch (EOFException e) {
            throw new AssertionError(what + ": expected invalid-data IOException, got EOF");
        } catch (IOException e) {
            return;
        }
        throw new AssertionError(what + ": expected IOException");
    }

    private static void expectBadInput(byte[] bytes, String what) {
        try {
            Proto.decodeInput(bytes);
        } catch (IOException e) {
            return;
        }
        throw new AssertionError(what + ": expected IOException");
    }

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    private static void eq(Object want, Object got, String what) {
        if (!want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"goldenControlOpenMatchesRust", (Case) ProtoTest::goldenControlOpenMatchesRust},
            {"goldenControlOpenWithTokenParses", (Case) ProtoTest::goldenControlOpenWithTokenParses},
            {"goldenControlFocusMatchesRust", (Case) ProtoTest::goldenControlFocusMatchesRust},
            {"goldenInputPointerMatchesRust", (Case) ProtoTest::goldenInputPointerMatchesRust},
            {"goldenInputKeyMatchesRust", (Case) ProtoTest::goldenInputKeyMatchesRust},
            {"goldenVideoKeyMatchesRust", (Case) ProtoTest::goldenVideoKeyMatchesRust},
            {"writeThenReadRoundTrip", (Case) ProtoTest::writeThenReadRoundTrip},
            {"cleanEofReturnsNull", (Case) ProtoTest::cleanEofReturnsNull},
            {"midFrameEofThrows", (Case) ProtoTest::midFrameEofThrows},
            {"invalidFramesRejected", (Case) ProtoTest::invalidFramesRejected},
            {"inputDecodeMirrorsRust", (Case) ProtoTest::inputDecodeMirrorsRust},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   ProtoTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL ProtoTest." + c[0] + ": " + t);
            }
        }
        System.out.println("ProtoTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
