package dev.lantern.helper;

import android.app.ActivityOptions;
import android.content.Intent;
import android.hardware.display.VirtualDisplay;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.SystemClock;
import android.view.Surface;

import com.genymobile.scrcpy.FakeContext;
import com.genymobile.scrcpy.display.DisplayInfo;
import com.genymobile.scrcpy.wrappers.ServiceManager;

import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

/**
 * One phone app on its own virtual display, encoded to a {@link VideoSink}. {@link #close()} quits the
 * app (removes every task on the display) and releases the display and encoder.
 */
public final class AppDisplay implements DisplayHandle {
    /** Pure policy (no Android classes): host tests call this. */
    static final class Policy {
        // android.hardware.display.DisplayManager.VIRTUAL_DISPLAY_FLAG_* (Global Constraints).
        static final int PUBLIC = 1;
        static final int PRESENTATION = 1 << 1;
        static final int OWN_CONTENT_ONLY = 1 << 3;
        static final int SUPPORTS_TOUCH = 1 << 6;
        static final int ROTATES_WITH_CONTENT = 1 << 7;
        static final int DESTROY_CONTENT_ON_REMOVAL = 1 << 8;
        // 1 << 9 is SHOULD_SHOW_SYSTEM_DECORATIONS: never set (Android would draw a taskbar).
        static final int TRUSTED = 1 << 10;
        static final int OWN_DISPLAY_GROUP = 1 << 11;
        static final int ALWAYS_UNLOCKED = 1 << 12;
        static final int TOUCH_FEEDBACK_DISABLED = 1 << 13;
        static final int OWN_FOCUS = 1 << 14;
        static final int DEVICE_DISPLAY_GROUP = 1 << 15;
        static final int FLAGS = PUBLIC | PRESENTATION | OWN_CONTENT_ONLY | SUPPORTS_TOUCH
            | ROTATES_WITH_CONTENT | DESTROY_CONTENT_ON_REMOVAL | TRUSTED | OWN_DISPLAY_GROUP
            | ALWAYS_UNLOCKED | TOUCH_FEEDBACK_DISABLED | OWN_FOCUS | DEVICE_DISPLAY_GROUP;
        /** WindowManager.DISPLAY_IME_POLICY_HIDE. */
        static final int IME_POLICY_HIDE = 2;

        private Policy() {
        }

        /** Size after the display goes from rotation {@code from} to {@code to} (Surface.ROTATION_*). */
        static int[] rotated(int w, int h, int from, int to) {
            return (from % 2) == (to % 2) ? new int[] {w, h} : new int[] {h, w};
        }
    }

    static final long TASK_WAIT_MS = 3000;
    static final long DRAIN_JOIN_MS = 1000;

    private final int win;
    private final String pkg;
    private final VideoSink sink;
    private final Encoder encoder;
    private final HandlerThread listenerThread;
    private VirtualDisplay display;
    private Thread drain;
    private android.hardware.display.DisplayManager.DisplayListener listener;
    private android.hardware.display.DisplayManager publicDm;
    private volatile ResizeListener resizeListener;
    private final Object failureLock = new Object();
    private StreamListener streamListener;
    private String failure;
    private int displayId = -1;
    private int taskId = -1;
    private int width;
    private int height;
    private int dpi;
    private int rotation;
    private boolean closed;

    private AppDisplay(int win, String pkg, VideoSink sink, int w, int h, int dpi) {
        this.win = win;
        this.pkg = pkg;
        this.sink = sink;
        this.width = w;
        this.height = h;
        this.dpi = dpi;
        this.encoder = new Encoder(Encoder.preferredMime(), w, h);
        this.listenerThread = new HandlerThread("lantern-display-" + win);
    }

    public static AppDisplay open(int win, String pkg, int user, int w, int h, int dpi, VideoSink sink) {
        AppDisplay d = new AppDisplay(win, pkg, sink, w, h, dpi);
        try {
            d.start(user);
            return d;
        } catch (UserLaunch.LaunchError e) {
            d.close();
            // Already user-facing ("<pkg> is not installed for Android user 10"): no wrapping.
            throw new IllegalArgumentException(e.getMessage());
        } catch (Exception e) {
            d.close();
            throw new IllegalStateException("open " + pkg + " win=" + win, e);
        }
    }

    private void start(int user) throws Exception {
        // Resolve first: a missing user or app fails before any display exists.
        UserLaunch.Target target = UserLaunch.resolve(pkg, user, new DeviceUsers());
        display = ServiceManager.getDisplayManager().createNewVirtualDisplay(
            "lantern-" + win + "-" + pkg, width, height, dpi, encoder.surface(), Policy.FLAGS);
        if (display == null || display.getDisplay() == null) {
            throw new IllegalStateException("createNewVirtualDisplay returned no display");
        }
        displayId = display.getDisplay().getDisplayId();
        ServiceManager.getWindowManager().setDisplayImePolicy(displayId, Policy.IME_POLICY_HIDE);
        int ime = ServiceManager.getWindowManager().getDisplayImePolicy(displayId);
        Droid.log("display: id=" + displayId + " " + width + "x" + height + "/" + dpi + " flags=0x"
            + Integer.toHexString(Policy.FLAGS) + " ime_policy=" + ime + " mime=" + encoder.mime() + " win=" + win);

        drain = new Thread(() -> {
            try {
                encoder.drainTo(sink, win, this::recoverEncoder, this::streamFailed);
            } catch (RuntimeException e) {
                streamFailed("drain: " + e);
            }
        }, "lantern-drain-" + win);
        // A sink stuck in a socket write must not keep the process alive after close abandons it.
        drain.setDaemon(true);
        drain.start();

        listenerThread.start();
        DisplayInfo info = ServiceManager.getDisplayManager().getDisplayInfo(displayId);
        rotation = info != null ? info.getRotation() : 0;
        // Public DisplayManager API, not scrcpy's DisplayManagerGlobal wrapper: on SDK 37 that
        // method's long argument is an INTERNAL_EVENT_FLAG_* mask and scrcpy's value (4) means
        // DISPLAY_ADDED, so it never delivers onDisplayChanged.
        publicDm = publicDisplayManager();
        listener = new android.hardware.display.DisplayManager.DisplayListener() {
            @Override
            public void onDisplayAdded(int id) {
            }

            @Override
            public void onDisplayRemoved(int id) {
            }

            @Override
            public void onDisplayChanged(int id) {
                if (id == displayId) {
                    try {
                        AppDisplay.this.onDisplayChanged();
                    } catch (RuntimeException e) {
                        Droid.log("rotation follow failed win=" + win + ": " + e);
                    }
                }
            }
        };
        publicDm.registerDisplayListener(listener, new Handler(listenerThread.getLooper()));

        launch(target);
        long deadline = SystemClock.elapsedRealtime() + TASK_WAIT_MS;
        while (true) {
            List<Integer> tasks = tasks();
            if (!tasks.isEmpty()) {
                taskId = tasks.get(0);
                break;
            }
            if (SystemClock.elapsedRealtime() > deadline) {
                throw new IllegalStateException("no task on display " + displayId + " after " + TASK_WAIT_MS + " ms");
            }
            Thread.sleep(50);
        }
        Droid.log("display: id=" + displayId + " task=" + taskId + " pkg=" + pkg);
    }

    private static android.hardware.display.DisplayManager publicDisplayManager()
            throws ReflectiveOperationException {
        java.lang.reflect.Constructor<android.hardware.display.DisplayManager> ctor =
            android.hardware.display.DisplayManager.class.getDeclaredConstructor(android.content.Context.class);
        ctor.setAccessible(true);
        return ctor.newInstance(FakeContext.get());
    }

    /** startActivityAsUser(target.user) with the activity resolved for that user (am start --user N). */
    private void launch(UserLaunch.Target target) throws Exception {
        int user = target.user;
        Intent intent = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
            .setComponent(new android.content.ComponentName(target.pkg, target.cls));
        // MULTIPLE_TASK: never pull an existing task (e.g. one on the phone's own screen) onto our
        // display, because close() removes every task here.
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_MULTIPLE_TASK);
        Bundle options = ActivityOptions.makeBasic().setLaunchDisplayId(displayId).toBundle();
        Object am = Class.forName("android.app.ActivityManager").getMethod("getService").invoke(null);
        Method start = am.getClass().getMethod("startActivityAsUser",
            Class.forName("android.app.IApplicationThread"), String.class, Intent.class, String.class,
            android.os.IBinder.class, String.class, int.class, int.class,
            Class.forName("android.app.ProfilerInfo"), Bundle.class, int.class);
        int result;
        try {
            result = (int) start.invoke(am, null, FakeContext.PACKAGE_NAME, intent, null, null, null, 0, 0,
                null, options, user);
        } catch (java.lang.reflect.InvocationTargetException e) {
            if (e.getCause() instanceof SecurityException) {
                throw new UserLaunch.LaunchError(UserLaunch.startFailure(pkg, user, e.getCause()));
            }
            throw e;
        }
        Droid.log("launch: " + target.flatten() + " user=" + user + " display=" + displayId + " result=" + result);
        if (result < 0) {
            throw new IllegalStateException("startActivityAsUser " + pkg + " result=" + result);
        }
    }

    private static Object atm() throws ReflectiveOperationException {
        return Class.forName("android.app.ActivityTaskManager").getMethod("getService").invoke(null);
    }

    /** Task ids on this display: IActivityTaskManager.getTasks(int, boolean, boolean, int). */
    private List<Integer> tasks() {
        List<Integer> ids = new ArrayList<>();
        if (displayId < 0) {
            return ids;
        }
        try {
            Object atm = atm();
            List<?> infos = (List<?>) atm.getClass()
                .getMethod("getTasks", int.class, boolean.class, boolean.class, int.class)
                .invoke(atm, 100, false, false, displayId);
            for (Object ti : infos) {
                Class<?> c = Class.forName("android.app.TaskInfo");
                if (c.getField("displayId").getInt(ti) == displayId) {
                    ids.add(c.getField("taskId").getInt(ti));
                }
            }
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException("getTasks", e);
        }
        return ids;
    }

    private static boolean removeTask(int id) {
        try {
            Object atm = atm();
            return (boolean) atm.getClass().getMethod("removeTask", int.class).invoke(atm, id);
        } catch (ReflectiveOperationException e) {
            Droid.log("removeTask " + id + " failed: " + e);
            return false;
        }
    }

    @Override
    public int displayId() {
        return displayId;
    }

    @Override
    public int taskId() {
        return taskId;
    }

    /** Opened.codec: {@code "h265"} for HEVC, else {@code "h264"}. */
    @Override
    public String codec() {
        return encoder.mime().equals(android.media.MediaFormat.MIMETYPE_VIDEO_HEVC) ? "h265" : "h264";
    }

    @Override
    public synchronized int width() {
        return width;
    }

    @Override
    public synchronized int height() {
        return height;
    }

    @Override
    public void setResizeListener(ResizeListener l) {
        resizeListener = l;
    }

    /**
     * Hears once when the stream dies (fatal codec error, failed recovery, or the sink threw). If the
     * stream already failed, {@code l} is told immediately. Not called for a normal {@link #close}.
     */
    @Override
    public void setStreamListener(StreamListener l) {
        String f;
        synchronized (failureLock) {
            streamListener = l;
            f = failure;
        }
        if (f != null && l != null) {
            l.streamFailed(win, f);
        }
    }

    /** Non-null once the stream failed (see {@link StreamListener}). */
    public String streamFailure() {
        synchronized (failureLock) {
            return failure;
        }
    }

    private void streamFailed(String reason) {
        StreamListener l;
        synchronized (failureLock) {
            if (failure != null) {
                return;
            }
            failure = reason;
            l = streamListener;
        }
        Droid.log("stream_failed win=" + win + " reason=" + reason);
        if (l != null) {
            l.streamFailed(win, reason);
        }
    }

    /** Recoverable codec error (drain thread): stop/configure/start at the current size, repoint the display. */
    private void recoverEncoder() {
        int w;
        int h;
        int d;
        synchronized (this) {
            w = width;
            h = height;
            d = dpi;
        }
        resize(w, h, d);
    }

    /** True when no task remains on the display (the app exited, e.g. Back on its root). */
    @Override
    public boolean taskVanished() {
        return tasks().isEmpty();
    }

    /** IDR on the next frame (focus, new subscriber). */
    @Override
    public void requestKeyframe() {
        encoder.requestSyncFrame();
    }

    /** Display gets exactly {@code w×h}; the encoder gets it aligned down to 16. */
    @Override
    public synchronized void resize(int w, int h, int newDpi) {
        if (closed) {
            throw new IllegalStateException("closed");
        }
        long t0 = SystemClock.elapsedRealtime();
        display.setSurface(null);
        Surface s = encoder.reconfigure(w, h);
        display.resize(w, h, newDpi);
        display.setSurface(s);
        encoder.requestSyncFrame();
        width = w;
        height = h;
        dpi = newDpi;
        Droid.log("resize: win=" + win + " " + w + "x" + h + "/" + newDpi + " setup_ms="
            + (SystemClock.elapsedRealtime() - t0));
    }

    /** The app rotated its display (ROTATES_WITH_CONTENT): follow it, the app wins (spec §9a). */
    private synchronized void onDisplayChanged() {
        if (closed) {
            return;
        }
        DisplayInfo info = ServiceManager.getDisplayManager().getDisplayInfo(displayId);
        if (info == null || info.getRotation() == rotation) {
            return;
        }
        int from = rotation;
        rotation = info.getRotation();
        int[] s = Policy.rotated(width, height, from, rotation);
        Droid.log("rotation: win=" + win + " " + from + "->" + rotation + " logical=" + info.getSize()
            + " size " + width + "x" + height + " -> " + s[0] + "x" + s[1]);
        // Back to 0 after we re-based the display to the app's orientation: nothing to do.
        if (rotation == 0 || (s[0] == width && s[1] == height)) {
            return;
        }
        resize(s[0], s[1], dpi);
        ResizeListener l = resizeListener;
        Droid.log("Resized win=" + win + " w=" + s[0] + " h=" + s[1]);
        if (l != null) {
            l.resized(win, s[0], s[1]);
        }
    }

    /** Removes every task on the display, then releases the display and encoder. Idempotent. */
    @Override
    public void close() {
        synchronized (this) {
            if (closed) {
                return;
            }
            closed = true;
        }
        try {
            for (int id : tasks()) {
                Droid.log("close: removeTask " + id + " -> " + removeTask(id));
            }
        } catch (RuntimeException e) {
            Droid.log("close: task removal failed: " + e);
        }
        if (listener != null) {
            publicDm.unregisterDisplayListener(listener);
        }
        listenerThread.quitSafely();
        if (display != null) {
            try {
                display.release();
            } catch (RuntimeException e) {
                Droid.log("close: display release failed: " + e);
            }
        }
        encoder.close();
        if (drain != null) {
            try {
                drain.join(DRAIN_JOIN_MS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            if (drain.isAlive()) {
                // Only a sink still blocked in packet() gets here (the loop itself has stopped).
                Droid.log("close: win=" + win + " drain still in sink after " + DRAIN_JOIN_MS
                    + " ms, interrupting and abandoning it");
                drain.interrupt();
            }
        }
        Droid.log("close: win=" + win + " display=" + displayId + " released");
    }
}
