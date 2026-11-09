package dev.lantern.helper;

import java.io.File;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.function.Consumer;

/**
 * The session {@link Journal} persisted on the phone ({@link #PATH}), so the originals survive
 * anything that kills both the helper and its {@link Cleanup} (SIGKILL of both, reboot, battery
 * death) and are never taken from a value an earlier session left behind. Written atomically and
 * durably (file fsync, rename, directory fsync) before any change, read by the next session, and
 * removed only after a clean restore ({@link Session#close} or {@link Cleanup#apply}).
 * Plain Java (host-testable with a temp dir and a fake {@link DirSync}).
 */
public final class JournalStore {
    public static final String PATH = "/data/local/tmp/lantern-session.json";

    /** Makes a rename in {@code dir} durable (fsync of the directory). */
    public interface DirSync {
        void sync(File dir) throws IOException;
    }

    private final File file;
    private final Consumer<String> log;
    private final DirSync dirSync;

    public JournalStore(File file, Consumer<String> log, DirSync dirSync) {
        this.file = file;
        this.log = log;
        this.dirSync = dirSync;
    }

    /** A store that persists nothing (tests and tools that do not touch the phone's settings). */
    public static JournalStore none() {
        return new JournalStore(null, msg -> { }, dir -> { });
    }

    public static JournalStore device(Consumer<String> log) {
        return new JournalStore(new File(PATH), log, JournalStore::fsyncDir);
    }

    /** Android only: {@code open(dir, O_RDONLY)}, {@code fsync}, {@code close}. */
    private static void fsyncDir(File dir) throws IOException {
        FileDescriptor fd;
        try {
            fd = android.system.Os.open(dir.getPath(), android.system.OsConstants.O_RDONLY, 0);
        } catch (Exception e) { // ErrnoException; not named so host tests never load it
            throw new IOException("open " + dir, e);
        }
        try {
            android.system.Os.fsync(fd);
        } catch (Exception e) {
            throw new IOException("fsync " + dir, e);
        } finally {
            try {
                android.system.Os.close(fd);
            } catch (Exception ignored) {
                // The sync result is what matters.
            }
        }
    }

    Consumer<String> log() {
        return log;
    }

    /** The persisted journal, or null if there is none or it cannot be read (logged). */
    public Journal load() {
        if (file == null || !file.exists()) {
            return null;
        }
        try {
            String json = new String(Files.readAllBytes(file.toPath()), StandardCharsets.UTF_8);
            return Journal.parse(json.trim());
        } catch (IOException | RuntimeException e) {
            log.accept("journal: " + file + " unreadable, ignoring it: " + e);
            return null;
        }
    }

    /**
     * Atomic and durable: written to a temp file, synced, renamed over {@link #PATH}, then the
     * directory synced so the rename itself survives a power cut.
     */
    public void save(Journal j) {
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
        File dir = file.getAbsoluteFile().getParentFile();
        try {
            dirSync.sync(dir);
        } catch (IOException | RuntimeException e) {
            throw new IllegalStateException("sync journal dir " + dir, e);
        }
    }

    /** Idempotent. Only after the originals are back. */
    public void clear() {
        if (file != null && file.exists() && !file.delete()) {
            log.accept("journal: could not delete " + file);
        }
    }
}
