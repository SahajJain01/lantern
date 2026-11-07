package dev.lantern.helper;

import java.io.BufferedReader;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;

/**
 * Out-of-process restore. {@link Main} spawns {@code app_process / dev.lantern.helper.Cleanup
 * <journal-json>} with a pipe on its stdin. When the pipe hits EOF (the parent died) the journal is
 * applied; if the parent first writes {@code done\n} (clean close) it exits without acting.
 *
 * <p>Everything except {@link #main} is plain Java so it runs in host-JVM tests.
 */
public final class Cleanup {
    static final String DONE = "done";

    private Cleanup() {
    }

    /** The state to restore: one setting, {@code orig == null} meaning the key was absent. */
    public static final class Journal {
        public final String ns;
        public final String key;
        public final String orig;

        public Journal(String ns, String key, String orig) {
            this.ns = ns;
            this.key = key;
            this.orig = orig;
        }

        /** Puts back the original value, or deletes the key if it was absent (never writes "null"). */
        void restore(SettingsBackend s) {
            if (orig == null) {
                s.delete(ns, key);
            } else {
                s.put(ns, key, orig);
            }
        }

        public String toJson() {
            return "{\"ns\":" + quote(ns) + ",\"key\":" + quote(key) + ",\"orig\":" + quote(orig) + "}";
        }

        public static Journal parse(String json) {
            Map<String, String> m = parseFlatObject(json);
            for (String k : new String[] {"ns", "key", "orig"}) {
                if (!m.containsKey(k)) {
                    throw new IllegalArgumentException("journal missing " + k + ": " + json);
                }
            }
            if (m.get("ns") == null || m.get("key") == null) {
                throw new IllegalArgumentException("journal ns/key null: " + json);
            }
            return new Journal(m.get("ns"), m.get("key"), m.get("orig"));
        }
    }

    /** Restores what the parent session changed: panel on, then the original setting. */
    public static void apply(Journal j, SettingsBackend s, Power p, Consumer<String> log) {
        try {
            p.setPanel(true);
            log.accept("cleanup: panel on");
        } catch (RuntimeException e) {
            log.accept("cleanup: panel on failed: " + e);
        }
        j.restore(s);
        log.accept("cleanup: restored " + j.ns + "/" + j.key + " to " + (j.orig == null ? "<absent>" : j.orig));
    }

    /** Blocks until the parent closes the pipe. False only if the parent said {@code done}. */
    public static boolean shouldRestore(InputStream in) {
        try {
            BufferedReader r = new BufferedReader(new InputStreamReader(in, StandardCharsets.UTF_8));
            String line;
            while ((line = r.readLine()) != null) {
                if (DONE.equals(line.trim())) {
                    return false;
                }
            }
        } catch (IOException e) {
            // Broken pipe: the parent is gone.
        }
        return true;
    }

    /** Spawns the Cleanup process; the returned guard writes {@code done} on disarm. */
    static Session.Guard spawnGuard(Consumer<String> log) {
        return new Session.Guard() {
            private Process process;

            @Override
            public void arm(String journalJson) {
                String cp = System.getenv("CLASSPATH");
                if (cp == null || cp.isEmpty()) {
                    throw new IllegalStateException("CLASSPATH not set; cannot spawn Cleanup");
                }
                ProcessBuilder b = new ProcessBuilder("app_process", "/", Cleanup.class.getName(), journalJson);
                b.environment().put("CLASSPATH", cp);
                // Never inherit our stdout/stderr pipes: after we die, writes there would SIGPIPE Cleanup.
                File devNull = new File("/dev/null");
                b.redirectOutput(devNull);
                b.redirectError(devNull);
                try {
                    process = b.start();
                } catch (IOException e) {
                    throw new IllegalStateException("spawn Cleanup", e);
                }
                log.accept("cleanup armed");
            }

            @Override
            public void disarm() {
                if (process == null) {
                    return;
                }
                try {
                    OutputStream out = process.getOutputStream();
                    out.write((DONE + "\n").getBytes(StandardCharsets.UTF_8));
                    out.close();
                } catch (IOException e) {
                    log.accept("cleanup disarm failed: " + e);
                }
                process = null;
            }
        };
    }

    public static void main(String[] args) {
        Droid.detach();
        Droid.prepare();
        if (args.length != 1) {
            Droid.log("cleanup: usage: Cleanup <journal-json>");
            System.exit(2);
        }
        Journal j;
        try {
            j = Journal.parse(args[0]);
        } catch (RuntimeException e) {
            Droid.log("cleanup: bad journal: " + e);
            System.exit(2);
            return;
        }
        Droid.log("cleanup: waiting, journal=" + args[0]);
        if (!shouldRestore(System.in)) {
            Droid.log("cleanup: parent closed cleanly, nothing to do");
            System.exit(0);
        }
        Droid.log("cleanup: parent died, restoring");
        try {
            apply(j, new ShellSettings(), new DevicePower(), Droid::log);
        } catch (RuntimeException e) {
            Droid.log("cleanup: restore failed: " + e);
            System.exit(1);
        }
        System.exit(0);
    }

    // --- minimal JSON for the journal: a flat object of string or null values ---

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

    static Map<String, String> parseFlatObject(String json) {
        int[] pos = {0};
        Map<String, String> m = new HashMap<>();
        skipWs(json, pos);
        expect(json, pos, '{');
        skipWs(json, pos);
        if (peek(json, pos) == '}') {
            pos[0]++;
        } else {
            while (true) {
                skipWs(json, pos);
                String k = parseString(json, pos);
                skipWs(json, pos);
                expect(json, pos, ':');
                skipWs(json, pos);
                String v;
                if (json.startsWith("null", pos[0])) {
                    pos[0] += 4;
                    v = null;
                } else {
                    v = parseString(json, pos);
                }
                m.put(k, v);
                skipWs(json, pos);
                char c = peek(json, pos);
                pos[0]++;
                if (c == '}') {
                    break;
                }
                if (c != ',') {
                    throw new IllegalArgumentException("bad json at " + (pos[0] - 1) + ": " + json);
                }
            }
        }
        skipWs(json, pos);
        if (pos[0] != json.length()) {
            throw new IllegalArgumentException("trailing json: " + json);
        }
        return m;
    }

    private static String parseString(String s, int[] pos) {
        expect(s, pos, '"');
        StringBuilder b = new StringBuilder();
        while (true) {
            char c = peek(s, pos);
            pos[0]++;
            if (c == '"') {
                return b.toString();
            }
            if (c != '\\') {
                b.append(c);
                continue;
            }
            char e = peek(s, pos);
            pos[0]++;
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
                    if (pos[0] + 4 > s.length()) {
                        throw new IllegalArgumentException("bad \\u escape: " + s);
                    }
                    b.append((char) Integer.parseInt(s.substring(pos[0], pos[0] + 4), 16));
                    pos[0] += 4;
                    break;
                default:
                    throw new IllegalArgumentException("bad escape \\" + e + ": " + s);
            }
        }
    }

    private static char peek(String s, int[] pos) {
        if (pos[0] >= s.length()) {
            throw new IllegalArgumentException("truncated json: " + s);
        }
        return s.charAt(pos[0]);
    }

    private static void expect(String s, int[] pos, char c) {
        if (peek(s, pos) != c) {
            throw new IllegalArgumentException("expected '" + c + "' at " + pos[0] + ": " + s);
        }
        pos[0]++;
    }

    private static void skipWs(String s, int[] pos) {
        while (pos[0] < s.length() && Character.isWhitespace(s.charAt(pos[0]))) {
            pos[0]++;
        }
    }
}
