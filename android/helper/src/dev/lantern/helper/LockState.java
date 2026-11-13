package dev.lantern.helper;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;

/**
 * Whether the phone's secure keyguard (PIN) is locked, from {@code dumpsys trust}: while it is,
 * Android returns null for every clipboard read and apps may not start (docs/protocol.md,
 * "Lock rules"). Only the {@code (current)} user's {@code deviceLocked=} counts; a work profile's
 * own line is ignored. Pure apart from {@link #device}, so host tests can drive it.
 *
 * <p>Unknown (no current user line, a failing or slow {@code dumpsys}) reads as not locked:
 * Lantern only reports a lock it is sure of.
 */
final class LockState {
    /** How long {@code dumpsys trust} may take before the state counts as unknown. */
    static final long DUMP_TIMEOUT_MS = 3000;

    /** The text of {@code dumpsys trust}. */
    interface Source {
        String dump() throws IOException;
    }

    private final Source source;
    private final Consumer<String> log;
    /** A failure was logged and no read has succeeded since: log one line per failing stretch. */
    private volatile boolean failing;

    LockState(Source source, Consumer<String> log) {
        this.source = source;
        this.log = log;
    }

    /** Reads the phone through {@code dumpsys trust} (bounded by {@link #DUMP_TIMEOUT_MS}). */
    static LockState device(Consumer<String> log) {
        return new LockState(() -> dumpsys(DUMP_TIMEOUT_MS, "trust"), log);
    }

    /**
     * The {@code deviceLocked=} value on the {@code (current)} user's line: true for 1, false for
     * 0, null when there is no such line or value.
     */
    static Boolean parseTrust(String dumpsys) {
        if (dumpsys == null) {
            return null;
        }
        for (String line : dumpsys.split("\n")) {
            if (!line.contains("(current)")) {
                continue;
            }
            int i = line.indexOf("deviceLocked=");
            if (i < 0) {
                return null;
            }
            int v = i + "deviceLocked=".length();
            if (v >= line.length()) {
                return null;
            }
            char c = line.charAt(v);
            boolean end = v + 1 >= line.length() || !Character.isLetterOrDigit(line.charAt(v + 1));
            if (c == '1' && end) {
                return Boolean.TRUE;
            }
            if (c == '0' && end) {
                return Boolean.FALSE;
            }
            return null;
        }
        return null;
    }

    /**
     * The current user's lock state: true locked, false unlocked, null unknown (no current user
     * line, or the source failed or timed out; logged once per failing stretch).
     */
    Boolean read() {
        Boolean b;
        try {
            b = parseTrust(source.dump());
        } catch (IOException | RuntimeException e) {
            if (!failing) {
                failing = true;
                log.accept("lock: reading dumpsys trust failed (" + e + "); lock state unknown");
            }
            return null;
        }
        failing = false;
        return b;
    }

    /** True only when the dump says the current user's device is locked; false when unknown. */
    boolean locked() {
        return Boolean.TRUE.equals(read());
    }

    /**
     * Device only: runs {@code dumpsys <args>}, killed after {@code timeoutMs}; at most 1 MiB of
     * output is kept. Also used by {@link FocusProbe#device}.
     */
    static String dumpsys(long timeoutMs, String... args) throws IOException {
        String[] cmd = new String[args.length + 1];
        cmd[0] = "dumpsys";
        System.arraycopy(args, 0, cmd, 1, args.length);
        String what = String.join(" ", cmd);
        Process p = new ProcessBuilder(cmd).redirectErrorStream(true).start();
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        Thread reader = new Thread(() -> {
            try (InputStream in = p.getInputStream()) {
                byte[] b = new byte[8192];
                int n;
                while ((n = in.read(b)) > 0) {
                    synchronized (buf) {
                        if (buf.size() < (1 << 20)) {
                            buf.write(b, 0, n);
                        }
                    }
                }
            } catch (IOException e) {
                // the process was killed or closed its output
            }
        }, "lantern-dumpsys");
        reader.setDaemon(true);
        reader.start();
        try {
            if (!p.waitFor(timeoutMs, TimeUnit.MILLISECONDS)) {
                p.destroyForcibly();
                throw new IOException(what + " took over " + timeoutMs + " ms");
            }
            reader.join(500);
        } catch (InterruptedException e) {
            p.destroyForcibly();
            Thread.currentThread().interrupt();
            throw new IOException("interrupted waiting for " + what);
        }
        synchronized (buf) {
            return new String(buf.toByteArray(), StandardCharsets.UTF_8);
        }
    }
}
