package dev.lantern.helper;

/** What {@link Router} needs from one open window; {@link AppDisplay} on the device, fakes in tests. */
public interface DisplayHandle extends AutoCloseable {
    int displayId();

    int taskId();

    /** {@code "h265"} or {@code "h264"}: the Opened.codec value. */
    String codec();

    int width();

    int height();

    /**
     * Width of the video the window shows: the encoder captures the top-left
     * {@code videoWidth()×videoHeight()} of the display (its size aligned down to 16), so
     * normalized pointer coordinates are fractions of this, not of {@link #width()}.
     */
    default int videoWidth() {
        return Math.min(width(), Encoder.Policy.align(width()));
    }

    default int videoHeight() {
        return Math.min(height(), Encoder.Policy.align(height()));
    }

    void resize(int w, int h, int dpi);

    boolean taskVanished();

    /**
     * True when the task on the display existed before the launch (Android moved it here from the
     * phone's screen or recents), so closing the window quits an app the user already had open.
     */
    default boolean moved() {
        return false;
    }

    /** True when the app runs in a managed (work) profile: {@code opened.work}. */
    default boolean work() {
        return false;
    }

    void requestKeyframe();

    void setStreamListener(StreamListener l);

    void setResizeListener(ResizeListener l);

    /** Idempotent, bounded (about 1 s). */
    @Override
    void close();
}
