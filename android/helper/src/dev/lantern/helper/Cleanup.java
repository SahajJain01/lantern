package dev.lantern.helper;

import java.io.BufferedReader;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;

/**
 * Out-of-process restore. {@link Main} spawns {@code app_process / dev.lantern.helper.Cleanup
 * <journal-json>} with a pipe on its stdin. When the pipe hits EOF (the parent died) the journal is
 * applied; if the parent first writes {@code done\n} (clean close) it exits without acting.
 * Startup handshake: once detached, prepared and holding a parsed journal, Cleanup writes
 * {@code ready\n} to its stdout pipe exactly once; the parent changes nothing until it reads it.
 *
 * <p>Everything except {@link #main} is plain Java so it runs in host-JVM tests.
 */
public final class Cleanup {
    static final String DONE = "done";
    /** Written to stdout exactly once, after setsid, prepare and journal parse. */
    static final String READY = "ready";

    private Cleanup() {
    }

    /** Restores what the parent session changed: panel on, then the journaled settings. */
    public static void apply(Journal j, SettingsBackend s, Power p, Consumer<String> log) {
        apply(j, s, p, JournalStore.none(), log);
    }

    /**
     * As above; the setting is restored even if the panel call throws (any Throwable), and the
     * persisted journal is removed only after the restore succeeded.
     */
    public static void apply(Journal j, SettingsBackend s, Power p, JournalStore store, Consumer<String> log) {
        try {
            p.setPanel(true);
            log.accept("cleanup: panel on");
        } catch (RuntimeException e) {
            log.accept("cleanup: panel on failed: " + e);
        } finally {
            j.restore(s, log);
            log.accept("cleanup: journal restored");
            store.clear();
        }
    }

    /**
     * {@code Main --forget} (undo-setup): restores a persisted journal (as {@link Journal#restore}:
     * only settings still holding Lantern's value), then deletes it and every Lantern artifact in
     * {@code dir} (helper jars, companion APKs). A failed restore throws and deletes nothing, so
     * the journal survives for a retry.
     *
     * @return {@code forgot journal=restored} or {@code forgot journal=none}
     */
    static String forget(JournalStore store, SettingsBackend s, File dir, Consumer<String> log) {
        Journal j = store.load();
        if (j != null) {
            j.restore(s, log);
        }
        store.clear();
        File[] files = dir.listFiles((d, name) -> (name.startsWith("lantern-helper-") && name.endsWith(".jar"))
            || (name.startsWith("lantern-companion-") && name.endsWith(".apk")));
        if (files != null) {
            for (File f : files) {
                if (!f.delete()) {
                    log.accept("forget: could not delete " + f);
                }
            }
        }
        return "forgot journal=" + (j != null ? "restored" : "none");
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
        return new ProcessGuard(ProcessGuard::launch, System.getenv("CLASSPATH"), READY_TIMEOUT_MS, log);
    }

    static final long READY_TIMEOUT_MS = 3000;

    /** Guard backed by a Cleanup child process. The launcher is injectable for host tests. */
    static final class ProcessGuard implements Session.Guard {
        interface Launcher {
            Process start(List<String> command, String classpath) throws IOException;
        }

        private final Launcher launcher;
        private final String classpath;
        private final long readyTimeoutMs;
        private final Consumer<String> log;
        private Process process;

        ProcessGuard(Launcher launcher, String classpath, long readyTimeoutMs, Consumer<String> log) {
            this.launcher = launcher;
            this.classpath = classpath;
            this.readyTimeoutMs = readyTimeoutMs;
            this.log = log;
        }

        /** Real launcher: stdin and stdout are pipes to us, stderr goes to /dev/null. */
        static Process launch(List<String> command, String classpath) throws IOException {
            ProcessBuilder b = new ProcessBuilder(command);
            b.environment().put("CLASSPATH", classpath);
            b.redirectError(new File("/dev/null"));
            return b.start();
        }

        @Override
        public void arm(String journalJson) {
            if (classpath == null || classpath.isEmpty()) {
                throw new IllegalStateException("CLASSPATH not set; cannot spawn Cleanup");
            }
            try {
                process = launcher.start(Arrays.asList("app_process", "/", Cleanup.class.getName(), journalJson), classpath);
            } catch (IOException e) {
                throw new IllegalStateException("spawn Cleanup", e);
            }
            String why = awaitReady(process.getInputStream(), readyTimeoutMs);
            if (why != null) {
                // Not provably alive and detached: refuse to start before anything is changed.
                process.destroyForcibly();
                process = null;
                throw new IllegalStateException("Cleanup not ready: " + why);
            }
            try {
                process.getInputStream().close(); // Cleanup never writes stdout again
            } catch (IOException ignored) {
                // Nothing to do.
            }
            log.accept("cleanup armed (ready)");
        }

        /** Reads Cleanup's first stdout line. Null if it is {@code ready}, else the failure reason. */
        static String awaitReady(InputStream in, long timeoutMs) {
            BlockingQueue<String> q = new ArrayBlockingQueue<>(1);
            Thread t = new Thread(() -> {
                StringBuilder b = new StringBuilder();
                String result;
                try {
                    int c;
                    while ((c = in.read()) != -1 && c != '\n' && b.length() < 64) {
                        b.append((char) c);
                    }
                    if (c == '\n' && READY.equals(b.toString().trim())) {
                        result = "";
                    } else {
                        result = c == -1 ? "exited before ready" : "unexpected output: " + b;
                    }
                } catch (IOException e) {
                    result = "read failed: " + e;
                }
                q.offer(result);
            }, "cleanup-ready");
            t.setDaemon(true);
            t.start();
            String r;
            try {
                r = q.poll(timeoutMs, TimeUnit.MILLISECONDS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return "interrupted";
            }
            if (r == null) {
                return "no ready within " + timeoutMs + " ms";
            }
            return r.isEmpty() ? null : r;
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
        // Handshake: we are detached, prepared and hold a valid journal. Then fd 1 is pointed at
        // /dev/null, so nothing can ever SIGPIPE us after the parent dies.
        System.out.print(READY + "\n");
        System.out.flush();
        Droid.silenceStdout();
        Droid.log("cleanup: ready, waiting, journal=" + args[0]);
        if (!shouldRestore(System.in)) {
            Droid.log("cleanup: parent closed cleanly, nothing to do");
            System.exit(0);
        }
        Droid.log("cleanup: parent died, restoring");
        try {
            apply(j, new ShellSettings(), new DevicePower(), JournalStore.device(Droid::log), Droid::log);
        } catch (RuntimeException | Error e) {
            Droid.log("cleanup: restore failed: " + e);
            System.exit(1);
        }
        System.exit(0);
    }
}
