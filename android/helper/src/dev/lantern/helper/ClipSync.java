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
    private final Consumer<String> toHost;
    private final Consumer<String> writePhone;
    /** The last text received from lanternd, until another phone change. Guarded by this. */
    private String lastFromHost;
    /** The clip timestamp of the last handled phone change (0 = none). Guarded by this. */
    private long lastStamp;

    /**
     * @param toHost sends {@code clip{text}} to lanternd
     * @param writePhone puts text on the phone's clipboard
     */
    ClipSync(Consumer<String> toHost, Consumer<String> writePhone) {
        this.toHost = toHost;
        this.writePhone = writePhone;
    }

    /**
     * The phone's clipboard changed to {@code text}; {@code stamp} is the clip's
     * {@code ClipDescription.getTimestamp()} (0 if unknown: never deduped). A repeat of the last
     * handled stamp is skipped. Sent unless null/empty, {@code sensitive} (ClipDescription extra
     * {@code android.content.extra.IS_SENSITIVE}) or over {@link Proto#MAX_CLIP} UTF-8 bytes (each
     * of which clears the host echo), or it is the echo of the host's text. A sent change clears the
     * host echo too.
     */
    void phoneChanged(String text, boolean sensitive, long stamp) {
        synchronized (this) {
            if (stamp != 0 && stamp == lastStamp) {
                return;
            }
            lastStamp = stamp;
            if (text == null || text.isEmpty() || sensitive || Router.utf8Length(text) > Proto.MAX_CLIP) {
                lastFromHost = null;
                return;
            }
            if (text.equals(lastFromHost)) {
                return;
            }
            lastFromHost = null;
        }
        toHost.accept(text);
    }

    /** lanternd sent {@code text} (the PC's clipboard): remember it, then put it on the phone. */
    void fromHost(String text) {
        synchronized (this) {
            lastFromHost = text;
        }
        writePhone.accept(text);
    }
}
