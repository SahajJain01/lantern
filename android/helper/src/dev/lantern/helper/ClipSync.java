package dev.lantern.helper;

import java.util.function.Consumer;

/**
 * The phone side of clipboard sync (docs/protocol.md, "Text, clipboard, catalogue rules"). Pure, so
 * host tests can drive it; {@link PhoneClipboard} feeds it on the device. Thread-safe: the phone's
 * listener and the router's control thread both call in.
 *
 * <p>Echo suppression: {@link #lastFromHost} is the text both sides last agreed on (lanternd's last
 * {@link #fromHost}, or the phone's last copy sent). The phone's change to that same text is not
 * sent: writing the host's text fires the phone's listener, and the phone may fire it more than once
 * per copy. Copying an older text again after the other side changed is still a change. Never logs
 * text.
 */
final class ClipSync {
    private final Consumer<String> toHost;
    private final Consumer<String> writePhone;
    /** The last text synced either way (received from lanternd or sent to it). Guarded by this. */
    private String lastFromHost;

    /**
     * @param toHost sends {@code clip{text}} to lanternd
     * @param writePhone puts text on the phone's clipboard
     */
    ClipSync(Consumer<String> toHost, Consumer<String> writePhone) {
        this.toHost = toHost;
        this.writePhone = writePhone;
    }

    /**
     * The phone's clipboard changed to {@code text}. Sent unless null/empty, {@code sensitive}
     * (ClipDescription extra {@code android.content.extra.IS_SENSITIVE}), over
     * {@link Proto#MAX_CLIP} UTF-8 bytes, or the text last synced either way.
     */
    void phoneChanged(String text, boolean sensitive) {
        if (text == null || text.isEmpty() || sensitive || Router.utf8Length(text) > Proto.MAX_CLIP) {
            return;
        }
        synchronized (this) {
            if (text.equals(lastFromHost)) {
                return;
            }
            lastFromHost = text;
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
