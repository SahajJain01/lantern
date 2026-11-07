package dev.lantern.helper;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.IOException;
import java.nio.charset.StandardCharsets;

/**
 * Control payloads: UTF-8 JSON objects tagged by {@code "t"} (docs/protocol.md). Uses the platform
 * org.json, so this class is device-only; host tests check raw JSON bytes via {@link Proto} instead.
 */
public final class Control {
    private Control() {
    }

    public static JSONObject decode(byte[] payload) throws IOException {
        try {
            JSONObject o = new JSONObject(new String(payload, StandardCharsets.UTF_8));
            o.getString("t");
            return o;
        } catch (JSONException e) {
            throw new IOException("bad control json: " + e.getMessage());
        }
    }

    public static byte[] encode(JSONObject msg) {
        return msg.toString().getBytes(StandardCharsets.UTF_8);
    }

    /** Builds {@code {"t":type}}; callers add fields with {@code put}. */
    public static JSONObject msg(String type) {
        try {
            return new JSONObject().put("t", type);
        } catch (JSONException e) {
            throw new AssertionError(e);
        }
    }
}
