package dev.lantern.helper;

/**
 * Told once when an {@link AppDisplay}'s video stream dies on its own: a fatal (or persistently
 * transient) codec error, a recovery that failed, or a {@link VideoSink} that threw. The display is
 * still open; the owner should report it to the host (Task 5: a Control message) and close it.
 * Called on the display's drain thread (or on the caller of {@code setStreamListener} if the failure
 * happened first); must not block.
 */
public interface StreamListener {
    void streamFailed(int win, String reason);
}
