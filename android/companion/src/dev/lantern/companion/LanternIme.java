package dev.lantern.companion;

import android.inputmethodservice.InputMethodService;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;
import android.util.Log;
import android.view.inputmethod.InputConnection;

/**
 * Lantern's hidden IME, selected by the helper for the length of a session: it draws nothing and
 * commits PC text ({@code ime} messages from the link) into the focused field. Hardware key events
 * pass through (no {@code onKeyDown} override). Every {@value #POLL_MS} ms it asks
 * {@link ImeState#WATCHDOG} whether to give the keyboard back after the helper's link was lost.
 * Never logs typed text: only {@code ime op=<op> ok=<bool> len=<n>}.
 */
public final class LanternIme extends InputMethodService {
    static final String ID = "dev.lantern.companion/.LanternIme";
    static final long POLL_MS = 500;

    private Handler handler;
    private final Runnable poll = new Runnable() {
        @Override
        public void run() {
            checkWatchdog();
            handler.postDelayed(this, POLL_MS);
        }
    };

    @Override
    public void onCreate() {
        super.onCreate();
        handler = new Handler(Looper.getMainLooper());
        ImeState.ime = this;
        handler.postDelayed(poll, POLL_MS);
        Log.i(Link.TAG, "ime created");
    }

    @Override
    public void onDestroy() {
        handler.removeCallbacks(poll);
        if (ImeState.ime == this) {
            ImeState.ime = null;
        }
        Log.i(Link.TAG, "ime destroyed");
        super.onDestroy();
    }

    /** Never show a soft keyboard on the phone: the text comes from the PC. */
    @Override
    public boolean onEvaluateInputViewShown() {
        return false;
    }

    @Override
    public boolean onEvaluateFullscreenMode() {
        return false;
    }

    /** Applies an {@code ime} op on the IME's thread (any thread may call it). */
    void post(String op, String text) {
        handler.post(() -> apply(op, text));
    }

    /** {@code commit}, {@code compose} or {@code finish} on the current input connection. */
    void apply(String op, String text) {
        InputConnection ic = getCurrentInputConnection();
        boolean ok;
        try {
            ok = ImeOps.apply(ic == null ? null : new ImeOps.Target() {
                @Override
                public boolean commit(String t) {
                    return ic.commitText(t, 1);
                }

                @Override
                public boolean compose(String t) {
                    return ic.setComposingText(t, 1);
                }

                @Override
                public boolean finish() {
                    return ic.finishComposingText();
                }
            }, op, text);
        } catch (RuntimeException e) {
            ok = false;
        }
        Log.i(Link.TAG, ImeOps.logLine(op, ok, text));
    }

    private void checkWatchdog() {
        boolean current;
        try {
            current = ID.equals(Settings.Secure.getString(getContentResolver(), Settings.Secure.DEFAULT_INPUT_METHOD));
        } catch (RuntimeException e) {
            current = false;
        }
        String prev = ImeState.WATCHDOG.due(SystemClock.elapsedRealtime(), current);
        if (prev == null) {
            return;
        }
        try {
            switchInputMethod(prev);
            Log.i(Link.TAG, "ime_restored");
        } catch (RuntimeException e) {
            Log.w(Link.TAG, "ime_restore failed: " + e);
        }
    }
}
