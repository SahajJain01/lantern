package dev.lantern.helper;

/** Input and focus for a window's display. Device: {@link Input}. */
public interface Injector {
    /** {@code w}/{@code h}: the display's current size, for scaling normalized pointer coordinates. */
    void inject(int displayId, int w, int h, Proto.InputEvent e);

    void focus(int taskId);
}
