package dev.lantern.helper;

import java.io.IOException;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;

/**
 * One control message (docs/protocol.md): the {@code "t"} tag plus flat fields whose values are
 * {@link String} or {@link Long}. Pure Java, so {@link Router} can be tested on the host JVM; the
 * JSON lives in a {@link ControlCodec} ({@link Control} on the device).
 */
public final class Msg {
    /** Every {@code t} protocol v1 defines. */
    public static final Set<String> TYPES = Collections.unmodifiableSet(new HashSet<>(Arrays.asList(
        "hello", "open", "opened", "resize", "resized", "focus", "close", "closed", "error", "bye")));

    public final String t;
    private final Map<String, Object> fields = new LinkedHashMap<>();

    private Msg(String t) {
        this.t = t;
    }

    public static Msg of(String t) {
        if (t == null) {
            throw new IllegalArgumentException("null t");
        }
        return new Msg(t);
    }

    /** {@code v} must be a String or an integral Number (stored as Long). */
    public Msg put(String key, Object v) {
        if (key.equals("t")) {
            throw new IllegalArgumentException("t is the tag");
        }
        if (v instanceof Integer || v instanceof Short || v instanceof Byte) {
            v = ((Number) v).longValue();
        }
        if (!(v instanceof String) && !(v instanceof Long)) {
            throw new IllegalArgumentException(key + ": unsupported value " + v);
        }
        fields.put(key, v);
        return this;
    }

    /** Insertion-ordered, read-only. */
    public Map<String, Object> fields() {
        return Collections.unmodifiableMap(fields);
    }

    public String str(String key) throws IOException {
        Object v = fields.get(key);
        if (!(v instanceof String)) {
            throw new IOException(t + "." + key + ": want string, got " + v);
        }
        return (String) v;
    }

    public long num(String key) throws IOException {
        Object v = fields.get(key);
        if (!(v instanceof Long)) {
            throw new IOException(t + "." + key + ": want integer, got " + v);
        }
        return (Long) v;
    }

    @Override
    public String toString() {
        return t + fields;
    }
}
