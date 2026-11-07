package dev.lantern.helper;

import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.nio.BufferUnderflowException;
import java.nio.ByteBuffer;

/**
 * Lantern wire protocol v1 (docs/protocol.md), mirroring host/proto/src/lib.rs.
 *
 * <p>Frame: {@code u32 len} (bytes after this field), {@code u8 kind}, {@code u32 stream},
 * {@code payload[len-5]}; all integers big-endian. Uses only java.* so it can be tested on the host
 * JVM. Control JSON (org.json) lives in {@link Control}.
 */
public final class Proto {
    public static final int VERSION = 1;

    public static final int KIND_CONTROL = 1;
    public static final int KIND_VIDEO = 2;
    public static final int KIND_INPUT = 3;

    /** Largest payload accepted on either side (8 MiB). */
    public static final int MAX_PAYLOAD = 8 << 20;
    private static final int HEADER_AFTER_LEN = 5;

    public static final int INPUT_POINTER = 1;
    public static final int INPUT_SCROLL = 2;
    public static final int INPUT_KEY = 3;

    public static final int POINTER_DOWN = 0;
    public static final int POINTER_UP = 1;
    public static final int POINTER_MOVE = 2;
    public static final int POINTER_HOVER_MOVE = 3;
    public static final int POINTER_CANCEL = 4;

    public static final int KEY_DOWN = 0;
    public static final int KEY_UP = 1;

    public static final int VIDEO_FLAG_CONFIG = 1;
    public static final int VIDEO_FLAG_KEY = 1 << 1;

    private Proto() {
    }

    /** One frame. {@code stream} is the wire u32 stored in an int (bit pattern preserved). */
    public static final class Frame {
        public final int kind;
        public final int stream;
        public final byte[] payload;

        public Frame(int kind, int stream, byte[] payload) {
            this.kind = kind;
            this.stream = stream;
            this.payload = payload;
        }
    }

    /**
     * Reads one frame. Returns {@code null} on clean EOF at a frame boundary; EOF anywhere inside a
     * frame (including inside {@code len}) throws {@link EOFException}. Invalid length or kind throws
     * {@link IOException} before the payload is read.
     */
    public static Frame read(DataInputStream in) throws IOException {
        int b0 = in.read();
        if (b0 < 0) {
            return null;
        }
        byte[] rest = new byte[3];
        in.readFully(rest);
        long len = ((long) b0 << 24) | ((rest[0] & 0xffL) << 16) | ((rest[1] & 0xffL) << 8) | (rest[2] & 0xffL);
        if (len < HEADER_AFTER_LEN) {
            throw new IOException("frame len " + len + " shorter than header");
        }
        long n = len - HEADER_AFTER_LEN;
        if (n > MAX_PAYLOAD) {
            throw new IOException("frame payload " + n + " exceeds MAX_PAYLOAD");
        }
        int kind = in.readUnsignedByte();
        int stream = in.readInt();
        checkKind(kind);
        byte[] payload = new byte[(int) n];
        in.readFully(payload);
        return new Frame(kind, stream, payload);
    }

    /** Writes and flushes one frame atomically with respect to other writers on {@code out}. */
    public static void write(DataOutputStream out, int kind, int stream, byte[] payload) throws IOException {
        checkKind(kind);
        if (payload.length > MAX_PAYLOAD) {
            throw new IOException("payload " + payload.length + " exceeds MAX_PAYLOAD");
        }
        synchronized (out) {
            out.writeInt(payload.length + HEADER_AFTER_LEN);
            out.writeByte(kind);
            out.writeInt(stream);
            out.write(payload);
            out.flush();
        }
    }

    private static void checkKind(int kind) throws IOException {
        if (kind != KIND_CONTROL && kind != KIND_VIDEO && kind != KIND_INPUT) {
            throw new IOException("unknown frame kind " + kind);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Video: u64 pts_us, u8 flags (bit0 config, bit1 key), codec bytes.

    public static byte[] encodeVideo(long ptsUs, boolean config, boolean key, byte[] data) {
        return encodeVideo(ptsUs, config, key, data, 0, data.length);
    }

    public static byte[] encodeVideo(long ptsUs, boolean config, boolean key, byte[] data, int off, int len) {
        ByteBuffer b = ByteBuffer.allocate(9 + len);
        b.putLong(ptsUs);
        b.put((byte) ((config ? VIDEO_FLAG_CONFIG : 0) | (key ? VIDEO_FLAG_KEY : 0)));
        b.put(data, off, len);
        return b.array();
    }

    /** Same as above from a buffer's remaining bytes (consumes them): one copy, straight from the codec. */
    public static byte[] encodeVideo(long ptsUs, boolean config, boolean key, ByteBuffer data) {
        ByteBuffer b = ByteBuffer.allocate(9 + data.remaining());
        b.putLong(ptsUs);
        b.put((byte) ((config ? VIDEO_FLAG_CONFIG : 0) | (key ? VIDEO_FLAG_KEY : 0)));
        b.put(data);
        return b.array();
    }

    // ---------------------------------------------------------------------------------------------
    // Input

    /** Base type of decoded input events. {@code x}/{@code y} are normalized 0..1 of the display. */
    public abstract static class InputEvent {
        InputEvent() {
        }
    }

    public static final class Pointer extends InputEvent {
        public final int action;
        public final long id;
        public final float x, y;
        public final int buttons;
        public final float pressure;

        public Pointer(int action, long id, float x, float y, int buttons, float pressure) {
            this.action = action;
            this.id = id;
            this.x = x;
            this.y = y;
            this.buttons = buttons;
            this.pressure = pressure;
        }
    }

    public static final class Scroll extends InputEvent {
        public final float x, y, h, v;

        public Scroll(float x, float y, float h, float v) {
            this.x = x;
            this.y = y;
            this.h = h;
            this.v = v;
        }
    }

    public static final class Key extends InputEvent {
        public final int action;
        public final int keycode, meta, repeat;

        public Key(int action, int keycode, int meta, int repeat) {
            this.action = action;
            this.keycode = keycode;
            this.meta = meta;
            this.repeat = repeat;
        }
    }

    /**
     * Decodes an Input payload. Unknown type/action, short payloads and trailing bytes all throw
     * {@link IOException} (Rust: {@code InvalidData}).
     */
    public static InputEvent decodeInput(byte[] payload) throws IOException {
        ByteBuffer b = ByteBuffer.wrap(payload); // big-endian by default
        InputEvent ev;
        try {
            int type = b.get() & 0xff;
            switch (type) {
                case INPUT_POINTER: {
                    int action = b.get() & 0xff;
                    if (action > POINTER_CANCEL) {
                        throw new IOException("unknown pointer action " + action);
                    }
                    ev = new Pointer(action, b.getLong(), b.getFloat(), b.getFloat(), b.getInt(), b.getFloat());
                    break;
                }
                case INPUT_SCROLL:
                    ev = new Scroll(b.getFloat(), b.getFloat(), b.getFloat(), b.getFloat());
                    break;
                case INPUT_KEY: {
                    int action = b.get() & 0xff;
                    if (action > KEY_UP) {
                        throw new IOException("unknown key action " + action);
                    }
                    ev = new Key(action, b.getInt(), b.getInt(), b.getInt());
                    break;
                }
                default:
                    throw new IOException("unknown input type " + type);
            }
        } catch (BufferUnderflowException e) {
            throw new IOException("payload too short");
        }
        if (b.hasRemaining()) {
            throw new IOException(b.remaining() + " trailing bytes in input event");
        }
        return ev;
    }
}
