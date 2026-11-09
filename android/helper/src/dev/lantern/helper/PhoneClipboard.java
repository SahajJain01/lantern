package dev.lantern.helper;

import android.content.ClipData;
import android.content.ClipDescription;
import android.content.Context;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.PersistableBundle;

import com.genymobile.scrcpy.FakeContext;

import java.lang.reflect.Field;
import java.util.function.Consumer;

/**
 * The phone's clipboard for {@link ClipSync}, on a dedicated {@code lantern-clip} HandlerThread
 * (Ruling 3): the helper never runs its main Looper, so a listener dispatched there would never
 * fire. The clipboard manager's dispatch handler is pointed at this thread too, because
 * {@code android.content.ClipboardManager} posts listener callbacks to the handler it was built with
 * (the fake ActivityThread's main-looper handler). Device only; never load from host tests.
 *
 * <p>Started from the router's session-up hook, stopped at teardown. Never logs clipboard text.
 */
final class PhoneClipboard implements AutoCloseable {
    static final String EXTRA_IS_SENSITIVE = "android.content.extra.IS_SENSITIVE";

    private final Consumer<String> log;
    private final HandlerThread thread = new HandlerThread("lantern-clip");
    private Handler handler;
    private android.content.ClipboardManager manager;
    private android.content.ClipboardManager.OnPrimaryClipChangedListener listener;
    private ClipSync sync;

    PhoneClipboard(Consumer<String> log) {
        this.log = log;
    }

    /** Starts the thread and registers the listener on it; changes go to {@code sync}. */
    synchronized void start(ClipSync sync) {
        if (handler != null) {
            return;
        }
        this.sync = sync;
        thread.start();
        handler = new Handler(thread.getLooper());
        handler.post(this::register);
    }

    /** Runs on lantern-clip. */
    private void register() {
        try {
            manager = (android.content.ClipboardManager) FakeContext.get().getSystemService(Context.CLIPBOARD_SERVICE);
            if (manager == null) {
                log.accept("clip: this phone has no clipboard service; clipboard sync off");
                return;
            }
            try {
                Field f = android.content.ClipboardManager.class.getDeclaredField("mHandler");
                f.setAccessible(true);
                f.set(manager, handler);
            } catch (ReflectiveOperationException | RuntimeException e) {
                log.accept("clip: cannot move the listener dispatch to lantern-clip (" + e
                    + "); phone copies may not reach the PC");
            }
            listener = this::changed;
            manager.addPrimaryClipChangedListener(listener);
            log.accept("clip: listening for phone clipboard changes");
        } catch (RuntimeException e) {
            log.accept("clip: listener registration failed: " + e);
        }
    }

    /** Runs on lantern-clip. */
    private void changed() {
        try {
            ClipData clip = manager.getPrimaryClip();
            if (clip == null || clip.getItemCount() == 0) {
                return;
            }
            CharSequence cs = clip.getItemAt(0).getText();
            String text = cs == null ? null : cs.toString();
            boolean sensitive = false;
            ClipDescription d = clip.getDescription();
            PersistableBundle extras = d == null ? null : d.getExtras();
            if (extras != null) {
                sensitive = extras.getBoolean(EXTRA_IS_SENSITIVE, false);
            }
            log.accept("clip: phone clipboard changed: " + (text == null ? "no text" : text.length() + " chars")
                + (sensitive ? ", sensitive (not sent)" : ""));
            sync.phoneChanged(text, sensitive);
        } catch (RuntimeException e) {
            log.accept("clip: reading the phone clipboard failed: " + e);
        }
    }

    /** Puts {@code text} on the phone's clipboard, from lantern-clip. */
    synchronized void write(String text) {
        if (handler == null) {
            return;
        }
        handler.post(() -> {
            if (manager == null) {
                return;
            }
            try {
                manager.setPrimaryClip(ClipData.newPlainText("lantern", text));
                log.accept("clip: PC clipboard (" + text.length() + " chars) set on the phone");
            } catch (RuntimeException e) {
                log.accept("clip: setting the phone clipboard failed: " + e);
            }
        });
    }

    /** Removes the listener and quits the thread (bounded wait). */
    @Override
    public synchronized void close() {
        if (handler == null) {
            return;
        }
        handler.post(() -> {
            if (manager != null && listener != null) {
                try {
                    manager.removePrimaryClipChangedListener(listener);
                } catch (RuntimeException e) {
                    log.accept("clip: listener removal failed: " + e);
                }
            }
        });
        thread.quitSafely();
        try {
            thread.join(1000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        handler = null;
    }
}
