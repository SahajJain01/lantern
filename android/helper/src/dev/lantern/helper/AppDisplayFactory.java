package dev.lantern.helper;

/** Opens a window's display. Device: {@code AppDisplay::open}. May call {@code sink} before returning. */
public interface AppDisplayFactory {
    DisplayHandle open(int win, String pkg, int user, int w, int h, int dpi, VideoSink sink) throws Exception;
}
