package dev.lantern.helper;

/** Phone power control: wake the device and switch the built-in panel (display 0). */
public interface Power {
    void wake();

    void setPanel(boolean on);
}
