package dev.lantern.helper;

/**
 * The app rotated its display and {@link AppDisplay} followed it; {@link Router} reports it as
 * {@code Resized{w,h}}. Called on the display's listener thread while it holds the display's lock:
 * must not block or call back into the display.
 */
public interface ResizeListener {
    void resized(int win, int w, int h);
}
