package dev.lantern.helper;

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.function.Consumer;

/**
 * Every phone setting a session changes: for each, the original ({@code orig}, null = the key was
 * absent) and the value Lantern wrote ({@code set}). Persisted by {@link JournalStore} and handed to
 * {@link Cleanup} before anything is changed. {@link #restore} puts an original back only while the
 * setting still holds the value Lantern wrote, so a change the user made since is never undone.
 *
 * <p>JSON: {@code {"v":2,"entries":[{"ns":..,"key":..,"orig":..|null,"set":..},...]}}. The Plan 1
 * flat form {@code {"ns","key","orig"}} is read as one entry with {@code set "7"}. Plain Java.
 */
public final class Journal {
    static final String IME_NS = "secure";
    static final String IME_KEY = "default_input_method";
    /** What a Plan 1 (flat) journal's session wrote: stay-awake on every power source. */
    static final String LEGACY_SET = "7";

    /** One changed setting. {@code orig == null}: absent before. {@code set}: what Lantern wrote. */
    public static final class Entry {
        public final String ns;
        public final String key;
        public final String orig;
        public final String set;

        public Entry(String ns, String key, String orig, String set) {
            this.ns = Objects.requireNonNull(ns, "ns");
            this.key = Objects.requireNonNull(key, "key");
            this.orig = orig;
            this.set = set;
        }

        boolean isIme() {
            return IME_NS.equals(ns) && IME_KEY.equals(key);
        }

        @Override
        public boolean equals(Object o) {
            if (!(o instanceof Entry)) {
                return false;
            }
            Entry e = (Entry) o;
            return ns.equals(e.ns) && key.equals(e.key) && Objects.equals(orig, e.orig) && Objects.equals(set, e.set);
        }

        @Override
        public int hashCode() {
            return Objects.hash(ns, key, orig, set);
        }

        @Override
        public String toString() {
            return ns + "/" + key + "{orig=" + orig + ", set=" + set + "}";
        }
    }

    private final List<Entry> entries;

    public Journal(List<Entry> entries) {
        this.entries = Collections.unmodifiableList(new ArrayList<>(entries));
    }

    public List<Entry> entries() {
        return entries;
    }

    /** The entry for {@code ns/key}, or null. */
    public Entry find(String ns, String key) {
        for (Entry e : entries) {
            if (e.ns.equals(ns) && e.key.equals(key)) {
                return e;
            }
        }
        return null;
    }

    /**
     * Newest change first. An entry whose setting still holds {@code set} gets its original back
     * (the IME through {@link SettingsBackend#selectIme}; an absent original is deleted, never
     * written as "null"); any other value was changed since the session started and is left. Every
     * entry is attempted; the first failure is rethrown after the last one.
     */
    public void restore(SettingsBackend s, Consumer<String> log) {
        RuntimeException first = null;
        for (int i = entries.size() - 1; i >= 0; i--) {
            Entry e = entries.get(i);
            try {
                String now = s.get(e.ns, e.key);
                if (!Objects.equals(now, e.set)) {
                    log.accept("journal: " + e.ns + "/" + e.key + " changed since the session started (now " + now
                        + "); left as is");
                    continue;
                }
                if (e.orig == null) {
                    s.delete(e.ns, e.key);
                } else if (e.isIme()) {
                    s.selectIme(e.orig);
                } else {
                    s.put(e.ns, e.key, e.orig);
                }
                log.accept("journal: restored " + e.ns + "/" + e.key + " to " + (e.orig == null ? "<absent>" : e.orig));
            } catch (RuntimeException ex) {
                log.accept("journal: restoring " + e.ns + "/" + e.key + " failed: " + ex);
                if (first == null) {
                    first = ex;
                }
            }
        }
        if (first != null) {
            throw first;
        }
    }

    public String toJson() {
        StringBuilder b = new StringBuilder("{\"v\":2,\"entries\":[");
        for (int i = 0; i < entries.size(); i++) {
            Entry e = entries.get(i);
            if (i > 0) {
                b.append(',');
            }
            b.append("{\"ns\":").append(quote(e.ns)).append(",\"key\":").append(quote(e.key))
                .append(",\"orig\":").append(quote(e.orig)).append(",\"set\":").append(quote(e.set)).append('}');
        }
        return b.append("]}").toString();
    }

    /** Parses the v2 form, or the Plan 1 flat form as a single stay-awake entry. */
    public static Journal parse(String json) {
        Parser p = new Parser(json);
        p.ws();
        p.expect('{');
        Map<String, Object> top = p.objectBody(true);
        p.ws();
        if (!p.atEnd()) {
            throw new IllegalArgumentException("trailing json: " + json);
        }
        if (!top.containsKey("v")) {
            Map<String, String> flat = strings(top, json);
            return new Journal(Collections.singletonList(entry(flat, false, json)));
        }
        if (!"2".equals(top.get("v")) || !(top.get("entries") instanceof List)) {
            throw new IllegalArgumentException("unsupported journal: " + json);
        }
        List<Entry> out = new ArrayList<>();
        for (Object o : (List<?>) top.get("entries")) {
            out.add(entry(strings(asMap(o, json), json), true, json));
        }
        return new Journal(out);
    }

    private static Entry entry(Map<String, String> m, boolean v2, String json) {
        String[] keys = v2 ? new String[] {"ns", "key", "orig", "set"} : new String[] {"ns", "key", "orig"};
        for (String k : keys) {
            if (!m.containsKey(k)) {
                throw new IllegalArgumentException("journal entry missing " + k + ": " + json);
            }
        }
        if (m.get("ns") == null || m.get("key") == null) {
            throw new IllegalArgumentException("journal ns/key null: " + json);
        }
        return new Entry(m.get("ns"), m.get("key"), m.get("orig"), v2 ? m.get("set") : LEGACY_SET);
    }

    @SuppressWarnings("unchecked")
    private static Map<String, Object> asMap(Object o, String json) {
        if (!(o instanceof Map)) {
            throw new IllegalArgumentException("journal entry not an object: " + json);
        }
        return (Map<String, Object>) o;
    }

    private static Map<String, String> strings(Map<String, Object> m, String json) {
        Map<String, String> out = new HashMap<>();
        for (Map.Entry<String, Object> e : m.entrySet()) {
            if (e.getValue() != null && !(e.getValue() instanceof String)) {
                throw new IllegalArgumentException("journal value of " + e.getKey() + " not a string: " + json);
            }
            out.put(e.getKey(), (String) e.getValue());
        }
        return out;
    }

    // --- minimal JSON: an object of string/null/integer values, and (top level only) one array of
    // such flat objects ---

    static String quote(String s) {
        if (s == null) {
            return "null";
        }
        StringBuilder b = new StringBuilder("\"");
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"':
                    b.append("\\\"");
                    break;
                case '\\':
                    b.append("\\\\");
                    break;
                case '\n':
                    b.append("\\n");
                    break;
                case '\r':
                    b.append("\\r");
                    break;
                case '\t':
                    b.append("\\t");
                    break;
                default:
                    if (c < 0x20) {
                        b.append(String.format("\\u%04x", (int) c));
                    } else {
                        b.append(c);
                    }
            }
        }
        return b.append('"').toString();
    }

    private static final class Parser {
        private final String s;
        private int pos;

        Parser(String s) {
            this.s = s;
        }

        boolean atEnd() {
            return pos == s.length();
        }

        /** After '{'. Values: string, null, a non-negative integer (as its text), or, if {@code nested}, an array of flat objects. */
        Map<String, Object> objectBody(boolean nested) {
            Map<String, Object> m = new HashMap<>();
            ws();
            if (peek() == '}') {
                pos++;
                return m;
            }
            while (true) {
                ws();
                String k = string();
                ws();
                expect(':');
                ws();
                Object v;
                char c = peek();
                if (s.startsWith("null", pos)) {
                    pos += 4;
                    v = null;
                } else if (c == '"') {
                    v = string();
                } else if (c >= '0' && c <= '9') {
                    int start = pos;
                    while (pos < s.length() && Character.isDigit(s.charAt(pos))) {
                        pos++;
                    }
                    v = s.substring(start, pos);
                } else if (c == '[' && nested) {
                    pos++;
                    v = array();
                } else {
                    throw new IllegalArgumentException("bad json value at " + pos + ": " + s);
                }
                if (m.containsKey(k)) {
                    throw new IllegalArgumentException("duplicate key " + k + ": " + s);
                }
                m.put(k, v);
                ws();
                c = peek();
                pos++;
                if (c == '}') {
                    return m;
                }
                if (c != ',') {
                    throw new IllegalArgumentException("bad json at " + (pos - 1) + ": " + s);
                }
            }
        }

        /** After '['. Flat objects only. */
        List<Object> array() {
            List<Object> out = new ArrayList<>();
            ws();
            if (peek() == ']') {
                pos++;
                return out;
            }
            while (true) {
                ws();
                expect('{');
                out.add(objectBody(false));
                ws();
                char c = peek();
                pos++;
                if (c == ']') {
                    return out;
                }
                if (c != ',') {
                    throw new IllegalArgumentException("bad json at " + (pos - 1) + ": " + s);
                }
            }
        }

        String string() {
            expect('"');
            StringBuilder b = new StringBuilder();
            while (true) {
                char c = peek();
                pos++;
                if (c == '"') {
                    return b.toString();
                }
                if (c != '\\') {
                    b.append(c);
                    continue;
                }
                char e = peek();
                pos++;
                switch (e) {
                    case '"':
                    case '\\':
                    case '/':
                        b.append(e);
                        break;
                    case 'n':
                        b.append('\n');
                        break;
                    case 'r':
                        b.append('\r');
                        break;
                    case 't':
                        b.append('\t');
                        break;
                    case 'b':
                        b.append('\b');
                        break;
                    case 'f':
                        b.append('\f');
                        break;
                    case 'u':
                        if (pos + 4 > s.length()) {
                            throw new IllegalArgumentException("bad \\u escape: " + s);
                        }
                        try {
                            b.append((char) Integer.parseInt(s.substring(pos, pos + 4), 16));
                        } catch (NumberFormatException ex) {
                            throw new IllegalArgumentException("bad \\u escape: " + s, ex);
                        }
                        pos += 4;
                        break;
                    default:
                        throw new IllegalArgumentException("bad escape \\" + e + ": " + s);
                }
            }
        }

        char peek() {
            if (pos >= s.length()) {
                throw new IllegalArgumentException("truncated json: " + s);
            }
            return s.charAt(pos);
        }

        void expect(char c) {
            if (peek() != c) {
                throw new IllegalArgumentException("expected '" + c + "' at " + pos + ": " + s);
            }
            pos++;
        }

        void ws() {
            while (pos < s.length() && Character.isWhitespace(s.charAt(pos))) {
                pos++;
            }
        }
    }
}
