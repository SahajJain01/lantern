package dev.lantern.helper;

import java.io.IOException;

/** Control payload JSON <-> {@link Msg}. Device: {@link Control#CODEC} (org.json). */
public interface ControlCodec {
    /** Throws {@link IOException} on bad JSON or a missing/non-string {@code t}. */
    Msg decode(byte[] payload) throws IOException;

    byte[] encode(Msg msg);
}
