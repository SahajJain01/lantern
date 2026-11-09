package dev.lantern.companion;

import dev.lantern.helper.ControlCodec;
import dev.lantern.helper.Msg;
import dev.lantern.helper.Proto;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;
import java.util.function.Consumer;

/**
 * The companion's side of the link protocol (docs/protocol.md, "Companion link"), over plain
 * streams so it runs on the host JVM: {@link #handshake}, then {@link #run} until EOF.
 * {@link #send} may be called from any thread. Never logs message fields (tokens, text).
 */
public final class LinkClient {
    /** Link-only types: they travel between the helper and the companion, never to lanternd. */
    static final Set<String> LINK_ONLY = Collections.unmodifiableSet(new HashSet<>(Arrays.asList("ime_prev", "sync")));

    /** Every {@code t} the companion accepts from the helper's codec. */
    static final Set<String> LINK_TYPES;

    static {
        Set<String> s = new HashSet<>(Msg.TYPES);
        s.addAll(LINK_ONLY);
        LINK_TYPES = Collections.unmodifiableSet(s);
    }

    /** What the helper sends over the link. Called on the link thread. */
    public interface Handler {
        /** The keyboard to restore if the link is lost ({@code ""}: none known). */
        void imePrev(String id);

        /**
         * A window's {@code ime{op, text}} (Task 7), with the link-only {@code display} of that
         * window ({@code -1} when absent).
         */
        void ime(int display, String op, String text);

        /** {@code sync}: send the full app catalogue (the helper's daemon session is up). */
        void sync();
    }

    private final DataInputStream in;
    private final DataOutputStream out;
    private final ControlCodec codec;
    private final Consumer<String> log;

    public LinkClient(InputStream in, OutputStream out, ControlCodec codec, Consumer<String> log) {
        this.in = new DataInputStream(new BufferedInputStream(in, 1 << 16));
        this.out = new DataOutputStream(new BufferedOutputStream(out, 1 << 16));
        this.codec = codec;
        this.log = log;
    }

    static final int SHELL_UID = 2000;
    static final int ROOT_UID = 0;

    /** Only the helper (shell, or root on a rooted phone) may hand the companion a link. */
    static boolean callerAllowed(int uid) {
        return uid == SHELL_UID || uid == ROOT_UID;
    }

    /** The {@code enabled} extra of a test-activity broadcast: true, false, or null (malformed). */
    static Boolean testActivityState(String enabled) {
        if ("true".equals(enabled)) {
            return Boolean.TRUE;
        }
        if ("false".equals(enabled)) {
            return Boolean.FALSE;
        }
        return null;
    }

    static boolean validToken(String s) {
        return s != null && s.matches("[0-9a-f]{32}");
    }

    /** Sends {@code hello{version, token}} and waits for the helper's {@code hello} reply. */
    public void handshake(String token) throws IOException {
        Proto.write(out, Proto.KIND_CONTROL, 0, codec.encode(
            Msg.of("hello").put("version", Proto.VERSION).put("token", token)));
        Proto.Frame f = Proto.read(in);
        if (f == null) {
            throw new EOFException("helper closed the link before its hello (wrong token?)");
        }
        if (f.kind != Proto.KIND_CONTROL || f.stream != 0) {
            throw new IOException("expected the helper's hello, got frame kind " + f.kind + " stream " + f.stream);
        }
        Msg m = codec.decode(f.payload);
        if (!m.t.equals("hello")) {
            throw new IOException("expected the helper's hello, got " + m.t);
        }
        if (m.num("version") != Proto.VERSION) {
            throw new IOException("helper speaks version " + m.num("version") + ", companion " + Proto.VERSION);
        }
    }

    /** Reads until EOF, handing {@code ime_prev}, {@code ime} and {@code sync} to {@code h}; anything else is dropped. */
    public void run(Handler h) throws IOException {
        while (true) {
            Proto.Frame f = Proto.read(in);
            if (f == null) {
                return;
            }
            if (f.kind != Proto.KIND_CONTROL || f.stream != 0) {
                log.accept("link: frame kind " + f.kind + " stream " + f.stream + " dropped");
                continue;
            }
            Msg m;
            try {
                m = codec.decode(f.payload);
            } catch (IOException e) {
                // Never the parser's message: it may quote the payload (typed text).
                log.accept("link: undecodable message of " + f.payload.length + " bytes dropped");
                continue;
            }
            try {
                switch (m.t) {
                    case "ime_prev":
                        h.imePrev(m.str("id"));
                        break;
                    case "ime":
                        int display = m.fields().get("display") instanceof Long ? (int) m.num("display") : -1;
                        h.ime(display, m.str("op"), m.fields().containsKey("text") ? m.str("text") : "");
                        break;
                    case "sync":
                        h.sync();
                        break;
                    default:
                        log.accept("link: unexpected " + m.t + " dropped");
                        break;
                }
            } catch (IOException e) {
                log.accept("link: malformed " + m.t + " dropped");
            }
        }
    }

    /** Sends {@code m} to the helper. False on a write failure. */
    public boolean send(Msg m) {
        try {
            Proto.write(out, Proto.KIND_CONTROL, 0, codec.encode(m));
            return true;
        } catch (IOException e) {
            log.accept("link: send " + m.t + " failed: " + e);
            return false;
        }
    }
}
