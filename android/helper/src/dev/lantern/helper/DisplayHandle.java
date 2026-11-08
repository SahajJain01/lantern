package dev.lantern.helper;

/** What {@link Router} needs from one open window; {@link AppDisplay} on the device, fakes in tests. */
public interface DisplayHandle extends AutoCloseable {
    int displayId();

    int taskId();

    /** {@code "h265"} or {@code "h264"}: the Opened.codec value. */
    String codec();

    int width();

    int height();

    void resize(int w, int h, int dpi);

    boolean taskVanished();

    /**
     * True when the task on the display existed before the launch (Android moved it here from the
     * phone's screen or recents), so closing the window quits an app the user already had open.
     */
    default boolean moved() {
        return false;
    }

    void requestKeyframe();

    void setStreamListener(StreamListener l);

    void setResizeListener(ResizeListener l);

    /** Idempotent, bounded (about 1 s). */
    @Override
    void close();
}
