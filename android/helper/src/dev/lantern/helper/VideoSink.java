package dev.lantern.helper;

import java.nio.ByteBuffer;

/** Receives encoded packets. {@code data} is only valid during the call (it is a codec buffer). */
public interface VideoSink {
    void packet(int win, long ptsUs, boolean config, boolean key, ByteBuffer data);
}
