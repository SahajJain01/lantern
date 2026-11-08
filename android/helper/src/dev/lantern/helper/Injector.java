package dev.lantern.helper;

/** Input and focus for a window's display. Device: {@link Input}. */
public interface Injector {
    /**
     * {@code w}/{@code h}: the size of the video the window shows ({@link DisplayHandle#videoWidth()},
     * the display's top-left, encoder-aligned part), for scaling normalized pointer coordinates.
     */
    void inject(int displayId, int w, int h, Proto.InputEvent e);

    void focus(int taskId);
}
