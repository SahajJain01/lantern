package dev.lantern.helper;

import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.Surface;

import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Surface-input video encoder for one app display. {@link #drainTo} runs on a per-display thread;
 * {@link #reconfigure} and {@link #close} may be called from any other thread.
 */
final class Encoder {
    static final int FPS = 60;
    static final int I_FRAME_INTERVAL_S = 10;
    /** KEY_REPEAT_PREVIOUS_FRAME_AFTER is in microseconds. */
    static final long REPEAT_AFTER_US = 100_000;
    private static final long DEQUEUE_TIMEOUT_US = 10_000;
    /** close() waits at most this long for the drain's in-flight dequeue before releasing anyway. */
    static final long CLOSE_LOCK_MS = 1000;
    /** Test hook (--display-test --recreate): skip the reuse path to measure the fallback. */
    static volatile boolean forceRecreate;

    /** Pure policy (no Android classes): host tests call this. */
    static final class Policy {
        static final String HEVC = "video/hevc";
        static final String AVC = "video/avc";
        static final String HEVC_HW = "c2.qti.hevc.encoder";
        static final String AVC_HW = "c2.qti.avc.encoder";
        static final int MIN_BPS = 4_000_000;
        static final int MAX_BPS = 20_000_000;

        private Policy() {
        }

        /** clamp(w*h*fps*0.1, 4, 20) Mbps. */
        static int bitrate(int w, int h, int fps) {
            long bps = (long) w * h * fps / 10;
            return (int) Math.max(MIN_BPS, Math.min(MAX_BPS, bps));
        }

        /** Down to a multiple of 16, never below 16. */
        static int align(int v) {
            return Math.max(16, v & ~15);
        }

        static final class Choice {
            final String mime;
            /** Codec name, or null to let MediaCodec pick an encoder by type. */
            final String name;

            Choice(String mime, String name) {
                this.mime = mime;
                this.name = name;
            }
        }

        /** HEVC on the Qualcomm encoder if listed, otherwise AVC (hardware if listed). */
        static Choice choose(List<String> encoderNames) {
            if (encoderNames.contains(HEVC_HW)) {
                return new Choice(HEVC, HEVC_HW);
            }
            return new Choice(AVC, encoderNames.contains(AVC_HW) ? AVC_HW : null);
        }
    }

    /** Names of every encoder on the device. */
    static List<String> encoderNames() {
        List<String> names = new ArrayList<>();
        for (MediaCodecInfo info : new MediaCodecList(MediaCodecList.REGULAR_CODECS).getCodecInfos()) {
            if (info.isEncoder()) {
                names.add(info.getName());
            }
        }
        return names;
    }

    /** The mime type {@link Policy#choose} picks on this device. */
    static String preferredMime() {
        return Policy.choose(encoderNames()).mime;
    }

    private final String mime;
    private final String name;
    private final ReentrantLock lock = new ReentrantLock(true);
    private final MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
    private MediaCodec codec;
    private Surface surface;
    private int width;
    private int height;
    private volatile boolean closed;
    /** Bumped by every reconfigure: packets copied from an older codec configuration are dropped. */
    private volatile int generation;
    /** Why there is no codec (recreate failed); null while healthy. Guarded by lock. */
    private String failure;
    private volatile DrainLoop loop;
    /** Drain-thread-only copy buffer; the sink runs after the codec buffer is released. */
    private ByteBuffer scratch = ByteBuffer.allocate(1 << 16);
    /** Set by reconfigure; cleared (and logged) at the first encoded frame afterwards. */
    private long resizeStartMs = -1;
    private String resizePath;

    /** Encoder size is {@code w×h} aligned down to 16. */
    Encoder(String mime, int w, int h) {
        this.mime = mime;
        Policy.Choice c = Policy.choose(encoderNames());
        this.name = mime.equals(c.mime) ? c.name : null;
        try {
            codec = create();
            surface = configureAndStart(codec, w, h);
        } catch (Exception e) {
            close();
            throw new IllegalStateException("encoder start " + mime + " " + w + "x" + h, e);
        }
    }

    String mime() {
        return mime;
    }

    int width() {
        return width;
    }

    int height() {
        return height;
    }

    /** Current input surface. Changes on {@link #reconfigure}. */
    Surface surface() {
        lock.lock();
        try {
            return surface;
        } finally {
            lock.unlock();
        }
    }

    private MediaCodec create() throws java.io.IOException {
        return name != null ? MediaCodec.createByCodecName(name) : MediaCodec.createEncoderByType(mime);
    }

    private MediaFormat format(int w, int h, boolean lowLatency) {
        MediaFormat f = MediaFormat.createVideoFormat(mime, w, h);
        f.setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
        f.setInteger(MediaFormat.KEY_BIT_RATE, Policy.bitrate(w, h, FPS));
        f.setInteger(MediaFormat.KEY_FRAME_RATE, FPS);
        f.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, I_FRAME_INTERVAL_S);
        f.setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0);
        f.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, REPEAT_AFTER_US);
        if (lowLatency) {
            f.setInteger(MediaFormat.KEY_LATENCY, 0);
            f.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
        }
        return f;
    }

    private boolean supportsLowLatency(MediaCodec c) {
        try {
            return c.getCodecInfo().getCapabilitiesForType(mime)
                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency);
        } catch (RuntimeException e) {
            return false;
        }
    }

    /** Configures {@code c} (fresh or stopped) at the aligned size, returns its new input surface. */
    private Surface configureAndStart(MediaCodec c, int w, int h) {
        int aw = Policy.align(w);
        int ah = Policy.align(h);
        boolean low = supportsLowLatency(c);
        try {
            c.configure(format(aw, ah, low), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
        } catch (RuntimeException e) {
            if (!low) {
                throw e;
            }
            Droid.log("encoder: low-latency configure rejected (" + e + "), retrying without");
            c.reset();
            c.configure(format(aw, ah, false), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
        }
        Surface s = c.createInputSurface();
        c.start();
        width = aw;
        height = ah;
        Droid.log("encoder: " + c.getName() + " " + mime + " " + aw + "x" + ah + " (asked " + w + "x" + h
            + ") bps=" + Policy.bitrate(aw, ah, FPS) + " low_latency=" + low);
        return s;
    }

    /**
     * Reconfigures to {@code w×h} (aligned) and returns the new input surface; the caller points the
     * display at it. Tries stop/configure on the same codec first, then release and recreate.
     */
    Surface reconfigure(int w, int h) {
        lock.lock();
        try {
            if (closed) {
                throw new IllegalStateException("encoder closed");
            }
            resizeStartMs = SystemClock.elapsedRealtime();
            generation++;
            Surface old = surface;
            surface = null;
            try {
                if (forceRecreate) {
                    throw new IllegalStateException("forced recreate");
                }
                codec.stop();
                surface = configureAndStart(codec, w, h);
                resizePath = "reuse";
            } catch (RuntimeException e) {
                Droid.log("encoder: reuse failed (" + e + "), recreating");
                try {
                    codec.release();
                } catch (RuntimeException ignored) {
                }
                codec = null;
                try {
                    codec = create();
                    surface = configureAndStart(codec, w, h);
                } catch (java.io.IOException | RuntimeException re) {
                    failure = "recreate " + w + "x" + h + " failed: " + re;
                    if (codec != null) {
                        try {
                            codec.release();
                        } catch (RuntimeException ignored) {
                        }
                        codec = null;
                    }
                    throw new IllegalStateException("encoder recreate", re);
                }
                resizePath = "recreate";
            }
            failure = null;
            if (old != null) {
                old.release();
            }
            return surface;
        } finally {
            lock.unlock();
        }
    }

    /** Asks for an IDR on the next frame (resize, focus, new subscriber). */
    void requestSyncFrame() {
        lock.lock();
        try {
            if (codec != null && !closed) {
                Bundle b = new Bundle();
                b.putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0);
                codec.setParameters(b);
            }
        } catch (IllegalStateException e) {
            Droid.log("encoder: sync frame request failed: " + e);
        } finally {
            lock.unlock();
        }
    }

    /** {@link #drainTo(VideoSink, int, DrainLoop.Recovery, DrainLoop.FailureListener)} without owner hooks. */
    void drainTo(VideoSink sink, int win) {
        drainTo(sink, win, null, null);
    }

    /**
     * Blocking loop (see {@link DrainLoop}) until {@link #close} or a reported failure. Each packet is
     * copied out and its codec buffer released under the lock; {@code sink} runs unlocked, so a sink
     * that blocks cannot stall reconfigure/close. {@code recovery} handles recoverable codec errors;
     * {@code failed} hears once about fatal codec errors and sink exceptions.
     */
    void drainTo(VideoSink sink, int win, DrainLoop.Recovery recovery, DrainLoop.FailureListener failed) {
        DrainLoop l = new DrainLoop(lock, new DrainLoop.Source() {
            @Override
            public DrainLoop.Packet dequeue() {
                return dequeueLocked(win);
            }

            @Override
            public DrainLoop.Fault classify(Exception e) {
                return Encoder.classify(e);
            }

            @Override
            public int generation() {
                return generation;
            }
        }, sink, win, recovery, failed);
        l.logger = Droid::log;
        loop = l;
        if (closed) {
            return;
        }
        l.run();
    }

    static DrainLoop.Fault classify(Exception e) {
        if (e instanceof MediaCodec.CodecException) {
            MediaCodec.CodecException ce = (MediaCodec.CodecException) e;
            if (ce.isTransient()) {
                return DrainLoop.Fault.TRANSIENT;
            }
            if (ce.isRecoverable()) {
                return DrainLoop.Fault.RECOVERABLE;
            }
        }
        // Plain IllegalStateException: the codec is released or in the error state. Reconfigure and
        // close hold the lock, so it is never a mid-reconfigure race.
        return DrainLoop.Fault.FATAL;
    }

    /** Lock held (DrainLoop). */
    private DrainLoop.Packet dequeueLocked(int win) {
        if (closed) {
            return DrainLoop.IDLE;
        }
        if (codec == null || surface == null) {
            if (failure != null) {
                throw new IllegalStateException("no codec: " + failure);
            }
            return DrainLoop.IDLE;
        }
        int index = codec.dequeueOutputBuffer(info, DEQUEUE_TIMEOUT_US);
        if (index < 0) {
            return null;
        }
        try {
            ByteBuffer data = codec.getOutputBuffer(index);
            if (data == null || info.size <= 0) {
                return null;
            }
            boolean config = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0;
            boolean key = (info.flags & MediaCodec.BUFFER_FLAG_KEY_FRAME) != 0;
            data.position(info.offset).limit(info.offset + info.size);
            if (scratch.capacity() < info.size) {
                scratch = ByteBuffer.allocate(Math.max(info.size, scratch.capacity() * 2));
            }
            scratch.clear();
            scratch.put(data);
            scratch.flip();
            if (!config && resizeStartMs >= 0) {
                Droid.log("resize_ms=" + (SystemClock.elapsedRealtime() - resizeStartMs)
                    + " path=" + resizePath + " size=" + width + "x" + height + " win=" + win);
                resizeStartMs = -1;
            }
            return new DrainLoop.Packet(generation, info.presentationTimeUs, config, key, scratch);
        } finally {
            codec.releaseOutputBuffer(index, false);
        }
    }

    /** Idempotent. */
    void close() {
        closed = true;
        DrainLoop l = loop;
        if (l != null) {
            l.stop();
        }
        boolean locked;
        try {
            locked = lock.tryLock(CLOSE_LOCK_MS, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            locked = false;
        }
        if (!locked) {
            Droid.log("encoder: close could not take the lock in " + CLOSE_LOCK_MS + " ms, releasing anyway");
        }
        try {
            if (codec != null) {
                try {
                    codec.stop();
                } catch (RuntimeException ignored) {
                }
                codec.release();
                codec = null;
            }
            if (surface != null) {
                surface.release();
                surface = null;
            }
        } finally {
            if (locked) {
                lock.unlock();
            }
        }
    }
}
