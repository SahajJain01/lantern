package dev.lantern.helper;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.function.Consumer;

/**
 * The session journal persisted on the phone ({@link #PATH}), so the original stay-awake value
 * survives anything that kills both the helper and its {@link Cleanup} (SIGKILL of both, reboot,
 * battery death) and is never taken from a value an earlier session left behind. Written
 * atomically before any change, read by the next session instead of the current value, and
 * removed only after a clean restore ({@link Session#close} or {@link Cleanup#apply}).
 * Plain Java (host-testable with a temp dir).
 */
public final class JournalStore {
    public static final String PATH = "/data/local/tmp/lantern-session.json";

    private final File file;
    private final Consumer<String> log;

    public JournalStore(File file, Consumer<String> log) {
        this.file = file;
        this.log = log;
    }

    /** A store that persists nothing (tests and tools that do not touch the phone's setting). */
    public static JournalStore none() {
        return new JournalStore(null, msg -> { });
    }

    public static JournalStore device(Consumer<String> log) {
        return new JournalStore(new File(PATH), log);
    }

    /** The persisted journal, or null if there is none or it cannot be read (logged). */
    public Cleanup.Journal load() {
        if (file == null || !file.exists()) {
            return null;
        }
        try {
            String json = new String(Files.readAllBytes(file.toPath()), StandardCharsets.UTF_8);
            return Cleanup.Journal.parse(json.trim());
        } catch (IOException | RuntimeException e) {
            log.accept("journal: " + file + " unreadable, ignoring it: " + e);
            return null;
        }
    }

    /** Atomic: written to a temp file, synced, then renamed over {@link #PATH}. */
    public void save(Cleanup.Journal j) {
        if (file == null) {
            return;
        }
        File tmp = new File(file.getPath() + ".tmp");
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(j.toJson().getBytes(StandardCharsets.UTF_8));
            out.getFD().sync();
        } catch (IOException e) {
            tmp.delete();
            throw new IllegalStateException("persist journal " + file, e);
        }
        if (!tmp.renameTo(file)) {
            tmp.delete();
            throw new IllegalStateException("rename " + tmp + " -> " + file);
        }
    }

    /** Idempotent. Only after the original value is back. */
    public void clear() {
        if (file != null && file.exists() && !file.delete()) {
            log.accept("journal: could not delete " + file);
        }
    }
}
