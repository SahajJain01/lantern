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

    void requestKeyframe();

    void setStreamListener(StreamListener l);

    void setResizeListener(ResizeListener l);

    /** Idempotent, bounded (about 1 s). */
    @Override
    void close();
}
