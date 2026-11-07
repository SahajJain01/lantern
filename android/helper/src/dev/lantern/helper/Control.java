package dev.lantern.helper;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Iterator;
import java.util.Map;

/**
 * Control payloads: UTF-8 JSON objects tagged by {@code "t"} (docs/protocol.md), via the platform
 * org.json, so this class is device-only. Host tests use an equivalent codec in the test tree.
 */
public final class Control {
    public static final ControlCodec CODEC = new ControlCodec() {
        @Override
        public Msg decode(byte[] payload) throws IOException {
            return Control.decode(payload);
        }

        @Override
        public byte[] encode(Msg msg) {
            return Control.encode(msg);
        }
    };

    private Control() {
    }

    /**
     * Strict on the tag ({@code t} must be a string naming a v1 message), lenient on fields: string
     * and integral values are kept, anything else (unknown extras such as nested objects) is dropped.
     */
    public static Msg decode(byte[] payload) throws IOException {
        JSONObject o;
        try {
            o = new JSONObject(new String(payload, StandardCharsets.UTF_8));
        } catch (JSONException e) {
            throw new IOException("bad control json: " + e.getMessage());
        }
        Object t = o.opt("t");
        if (!(t instanceof String)) {
            throw new IOException("control json without a string t");
        }
        if (!Msg.TYPES.contains(t)) {
            throw new IOException("unknown control type " + t);
        }
        Msg m = Msg.of((String) t);
        for (Iterator<String> it = o.keys(); it.hasNext();) {
            String k = it.next();
            if (k.equals("t")) {
                continue;
            }
            Object v = o.opt(k);
            if (v instanceof String) {
                m.put(k, v);
            } else if (v instanceof Integer || v instanceof Long) {
                m.put(k, ((Number) v).longValue());
            } else if (v instanceof Number) {
                double d = ((Number) v).doubleValue();
                if (d == Math.rint(d) && Math.abs(d) < 1e15) {
                    m.put(k, (long) d);
                }
            }
        }
        return m;
    }

    public static byte[] encode(Msg msg) {
        try {
            JSONObject o = new JSONObject().put("t", msg.t);
            for (Map.Entry<String, Object> e : msg.fields().entrySet()) {
                o.put(e.getKey(), e.getValue());
            }
            return o.toString().getBytes(StandardCharsets.UTF_8);
        } catch (JSONException e) {
            throw new AssertionError(e);
        }
    }
}
