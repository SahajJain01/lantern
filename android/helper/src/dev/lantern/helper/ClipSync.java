package dev.lantern.helper;

import java.util.function.Consumer;

/**
 * The phone side of clipboard sync (docs/protocol.md, "Text, clipboard, catalogue rules"). Pure, so
 * host tests can drive it; {@link PhoneClipboard} feeds it on the device. Thread-safe: the phone's
 * listener and the router's control thread both call in.
 *
 * <p>Echo suppression: only the echo of the text received from lanternd ({@link #lastFromHost}) is
 * suppressed, because writing it fires the phone's listener. The phone's own sent text is not
 * remembered (a send can be dropped on the PC side, and the helper can't know). Any other phone
 * change, synced or not (sensitive, oversize, null/empty), clears {@link #lastFromHost}, so the
 * host's text copied again later is sent. The listener fires twice per clip on some phones: a
 * change with the same {@code ClipDescription.getTimestamp()} as the last handled one is skipped.
 * Never logs text.
 */
final class ClipSync {
    /** {@link LockState} for a phone that is never reported locked. */
    private static final LockState NEVER_LOCKED = new LockState(() -> "", s -> { });

    private final Consumer<String> toHost;
    private final Consumer<String> writePhone;
    private final Consumer<String> writeHeld;
    private final LockState lock;
    private final Consumer<String> onLocked;
    /** The last text received from lanternd, until another phone change. Guarded by this. */
    private String lastFromHost;
    /** The clip timestamp of the last handled phone change (0 = none). Guarded by this. */
    private long lastStamp;
    /** The latest PC copy that arrived while the phone was locked, until unlock. Guarded by this. */
    private String held;

    /**
     * @param toHost sends {@code clip{text}} to lanternd
     * @param writePhone puts text on the phone's clipboard
     */
    ClipSync(Consumer<String> toHost, Consumer<String> writePhone) {
        this(toHost, writePhone, writePhone, NEVER_LOCKED, reason -> { });
    }

    /**
     * Clipboard sync aware of the PIN lock (Android returns null for every clipboard read while
     * it is locked, and a write would be unreadable until unlock).
     *
     * @param writeHeld puts a held PC copy on the phone's clipboard after unlock
     * @param lock read on each PC copy and on each phone change that reads back null
     * @param onLocked told {@code "clipboard"} each time the lock got in the way (the caller
     *     dedupes per locked stretch)
     */
    ClipSync(Consumer<String> toHost, Consumer<String> writePhone, Consumer<String> writeHeld, LockState lock,
            Consumer<String> onLocked) {
        this.toHost = toHost;
        this.writePhone = writePhone;
        this.writeHeld = writeHeld;
        this.lock = lock;
        this.onLocked = onLocked;
    }

    /**
     * The phone's clipboard changed to {@code text}; {@code stamp} is the clip's
     * {@code ClipDescription.getTimestamp()} (0 if unknown: never deduped). A repeat of the last
     * handled stamp is skipped. Sent unless null/empty, {@code sensitive} (ClipDescription extra
     * {@code android.content.extra.IS_SENSITIVE}) or over {@link Proto#MAX_CLIP} UTF-8 bytes (each
     * of which clears the host echo), or it is the echo of the host's text. A sent change clears the
     * host echo too. A null read while the phone is locked also reports {@code "clipboard"} to
     * {@code onLocked}: the lock, not an empty clip, hid the copy.
     */
    void phoneChanged(String text, boolean sensitive, long stamp) {
        synchronized (this) {
            if (stamp != 0 && stamp == lastStamp) {
                return;
            }
            lastStamp = stamp;
            if (text != null) {
                held = null; // the phone is readable and its copy is newer than any held PC copy
            }
            if (text == null || text.isEmpty() || sensitive || Router.utf8Length(text) > Proto.MAX_CLIP) {
                lastFromHost = null;
                if (text != null) {
                    return;
                }
            } else if (text.equals(lastFromHost)) {
                return;
            } else {
                lastFromHost = null;
            }
        }
        if (text == null) {
            if (lock.locked()) {
                onLocked.accept("clipboard");
            }
            return;
        }
        toHost.accept(text);
    }

    /**
     * lanternd sent {@code text} (the PC's clipboard): remember it, then put it on the phone. While
     * the phone is locked nothing is written: the text is held (only the latest) for
     * {@link #unlocked}, and {@code onLocked} hears {@code "clipboard"}.
     */
    void fromHost(String text) {
        if (lock.locked()) {
            synchronized (this) {
                held = text;
            }
            onLocked.accept("clipboard");
            return;
        }
        synchronized (this) {
            held = null; // a newer PC copy supersedes one held earlier
            lastFromHost = text;
        }
        writePhone.accept(text);
    }

    /** The phone was unlocked: writes the held PC copy, if any (its echo is suppressed), then forgets it. */
    void unlocked() {
        String text;
        synchronized (this) {
            text = held;
            held = null;
            if (text == null) {
                return;
            }
            lastFromHost = text;
        }
        writeHeld.accept(text);
    }
}
