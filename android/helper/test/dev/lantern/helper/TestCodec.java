package dev.lantern.helper;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Map;

/**
 * Host-JVM stand-in for {@link Control}'s org.json codec: flat JSON objects whose values are strings
 * or integers. Test utility only (not a *Test class, so the build does not run it).
 */
final class TestCodec implements ControlCodec {
    @Override
    public Msg decode(byte[] payload) throws IOException {
        String s = new String(payload, StandardCharsets.UTF_8).trim();
        if (!s.startsWith("{") || !s.endsWith("}")) {
            throw new IOException("not an object: " + s);
        }
        String body = s.substring(1, s.length() - 1).trim();
        Msg m = null;
        java.util.LinkedHashMap<String, Object> fields = new java.util.LinkedHashMap<>();
        int i = 0;
        while (i < body.length()) {
            int k0 = body.indexOf('"', i);
            int k1 = body.indexOf('"', k0 + 1);
            String key = body.substring(k0 + 1, k1);
            int colon = body.indexOf(':', k1);
            int v = colon + 1;
            Object value;
            int end;
            if (body.charAt(v) == '"') {
                end = body.indexOf('"', v + 1);
                value = body.substring(v + 1, end);
                end++;
            } else {
                end = body.indexOf(',', v);
                if (end < 0) {
                    end = body.length();
                }
                value = Long.parseLong(body.substring(v, end).trim());
            }
            fields.put(key, value);
            int comma = body.indexOf(',', end);
            i = comma < 0 ? body.length() : comma + 1;
        }
        Object t = fields.remove("t");
        if (!(t instanceof String)) {
            throw new IOException("missing t: " + s);
        }
        m = Msg.of((String) t);
        for (Map.Entry<String, Object> e : fields.entrySet()) {
            m.put(e.getKey(), e.getValue());
        }
        return m;
    }

    @Override
    public byte[] encode(Msg m) {
        StringBuilder b = new StringBuilder("{\"t\":\"").append(m.t).append('"');
        for (Map.Entry<String, Object> e : m.fields().entrySet()) {
            b.append(",\"").append(e.getKey()).append("\":");
            Object v = e.getValue();
            if (v instanceof String) {
                b.append('"').append(v).append('"');
            } else {
                b.append(v);
            }
        }
        return b.append('}').toString().getBytes(StandardCharsets.UTF_8);
    }
}
