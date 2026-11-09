package dev.lantern.companion;

import android.inputmethodservice.InputMethodService;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;
import android.util.Log;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodInfo;
import android.view.inputmethod.InputMethodManager;

/**
 * Lantern's hidden IME, selected by the helper for the length of a session: it draws nothing and
 * commits PC text ({@code ime} messages from the link) into the focused field. Hardware key events
 * pass through (no {@code onKeyDown} override). Every {@value #POLL_MS} ms it asks
 * {@link ImeState#WATCHDOG} whether to give the keyboard back after the helper's link was lost, or
 * after it was created with no link up (a reboot or battery death mid-session).
 * Never logs typed text: only {@code ime op=<op> ok=<bool> len=<n>}.
 */
public final class LanternIme extends InputMethodService {
    static final String ID = ImeWatchdog.LANTERN_ID;
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
        PrefsPrevStore.install(this);
        // Created with no helper link (e.g. the phone rebooted mid-session and the system bound the
        // still-selected Lantern IME): nothing else will give the keyboard back, so arm the
        // watchdog with the persisted ime_prev (or none: then the first other enabled keyboard).
        String prev = ImeState.restoreTarget();
        boolean armed = ImeState.WATCHDOG.createdUnlinked(SystemClock.elapsedRealtime(), prev);
        handler.postDelayed(poll, POLL_MS);
        Log.i(Link.TAG, "ime created display=" + imeDisplayId() + (armed ? "; no helper link: the keyboard is given back in "
            + ImeWatchdog.DEFAULT_GRACE_MS + " ms unless the helper links (original "
            + (prev != null ? "known" : "unknown: fallback") + ")" : ""));
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

    /** Applies an {@code ime} op for {@code display} on the IME's thread (any thread may call it). */
    void post(int display, String op, String text) {
        handler.post(() -> apply(display, op, text));
    }

    /** Logged once per IME instance when its display cannot be determined. */
    private boolean unknownDisplayLogged;

    /**
     * The display this IME instance serves, or -1 if unknown. Under the IME display policy
     * {@code LOCAL} the IME lives on the focused editor's display (it is recreated as focus moves
     * between displays); InputMethodService is a window context, so its own display says which.
     * Its window's decor view, when attached, is checked first.
     */
    private int imeDisplayId() {
        try {
            android.app.Dialog d = getWindow();
            android.view.Window w = d == null ? null : d.getWindow();
            android.view.View decor = w == null ? null : w.peekDecorView();
            android.view.Display disp = decor == null ? null : decor.getDisplay();
            if (disp != null) {
                return disp.getDisplayId();
            }
        } catch (RuntimeException ignored) {
            // fall through to the context's display
        }
        try {
            android.view.Display disp = getDisplay();
            if (disp != null) {
                return disp.getDisplayId();
            }
        } catch (RuntimeException ignored) {
            // UnsupportedOperationException: not a visual context
        }
        return -1;
    }

    /**
     * {@code commit}, {@code compose} or {@code finish} on the current input connection, only if it
     * belongs to {@code display} (the window the text was typed in): right after a window switch
     * the IME may still be bound to another Lantern window's editor. An unknown display on either
     * side lets it through (typing never breaks), logged once.
     */
    void apply(int display, String op, String text) {
        int mine = imeDisplayId();
        if (mine < 0 && display >= 0 && !unknownDisplayLogged) {
            unknownDisplayLogged = true;
            Log.w(Link.TAG, "ime: cannot tell this keyboard's display; applying ime without the display check");
        }
        if (!ImeOps.displayMatches(display, mine)) {
            Log.i(Link.TAG, ImeOps.droppedLine(op, text, display, mine));
            return;
        }
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
        ImeWatchdog.Action a = ImeState.WATCHDOG.due(SystemClock.elapsedRealtime(), current, this::enabledIds);
        if (a == null) {
            return;
        }
        if (a.done) {
            ImeState.restored(); // nothing left to give back: the persisted ime_prev is cleared
            Log.i(Link.TAG, "ime_restore done");
            return;
        }
        if (a.id == null) {
            Log.w(Link.TAG, "ime_restore gave up: " + a.giveUp);
            return;
        }
        // A switch that throws or does not take is retried (then falls back) by the watchdog.
        try {
            switchInputMethod(a.id);
            Log.i(Link.TAG, a.fallback ? "ime_restored fallback=" + a.id : "ime_restored");
        } catch (RuntimeException e) {
            Log.w(Link.TAG, "ime_restore failed" + (a.fallback ? " (fallback)" : "") + ": " + e);
        }
    }

    /** The enabled IME ids (InputMethodInfo.getId), in the system's order. */
    private java.util.List<String> enabledIds() {
        java.util.List<String> ids = new java.util.ArrayList<>();
        InputMethodManager imm = getSystemService(InputMethodManager.class);
        for (InputMethodInfo info : imm.getEnabledInputMethodList()) {
            ids.add(info.getId());
        }
        return ids;
    }
}
