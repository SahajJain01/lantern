package dev.lantern.companion;

import android.content.Context;
import android.os.SystemClock;
import android.util.Log;

import dev.lantern.helper.Control;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/**
 * The companion's running link to the helper: one thread that does the hello over the streams
 * {@link LinkProvider} was handed, then reads frames until EOF. {@link #start} replaces any running
 * link. Never logs the token or any text it carries.
 */
final class Link {
    static final String TAG = "LanternCompanion";

    private static Link running; // guarded by Link.class

    private final Context context;
    private final InputStream in;
    private final OutputStream out;
    private final String token;
    private volatile boolean stopped;
    private volatile LinkClient client;
    /** Set once the hello succeeded (only a link that was up can be lost). */
    private volatile boolean linked;

    private Link(Context context, InputStream in, OutputStream out, String token) {
        this.context = context;
        this.in = in;
        this.out = out;
        this.token = token;
    }

    /** Stops any running link and starts a new one over {@code in}/{@code out}. */
    static synchronized void start(Context context, InputStream in, OutputStream out, String token) {
        if (running != null) {
            running.stop();
        }
        Link l = new Link(context, in, out, token);
        running = l;
        Thread t = new Thread(l::run, "lantern-link");
        t.setDaemon(true);
        t.start();
    }

    /** The linked client, or null. */
    static synchronized LinkClient current() {
        return running == null ? null : running.client;
    }

    private void stop() {
        stopped = true;
        try {
            out.close();
        } catch (IOException | RuntimeException ignored) {
            // closed
        }
        try {
            in.close();
        } catch (IOException | RuntimeException ignored) {
            // closed
        }
    }

    private void run() {
        LauncherCatalog catalog = null;
        try {
            LinkClient c = new LinkClient(in, out, Control.codec(LinkClient.LINK_TYPES), msg -> Log.i(TAG, msg));
            c.handshake(token);
            client = c;
            linked = true;
            // A new session within the grace keeps its keyboard, and the previous session's
            // ime_prev is marked stale (this session sends its own if it selects the Lantern IME;
            // the persisted copy stays the restore target until then).
            PrefsPrevStore.install(context);
            ImeState.linkUp();
            Log.i(TAG, "linked to the helper");
            // Callbacks are registered now; the catalogue is sent when the helper asks (sync).
            // Without it (LauncherApps refused) the link still serves the IME.
            LauncherCatalog made = null;
            try {
                made = new LauncherCatalog(context, c::send);
            } catch (RuntimeException e) {
                Log.w(TAG, "catalog unavailable: " + e);
            }
            final LauncherCatalog cat = made;
            catalog = cat;
            c.run(new LinkClient.Handler() {
                @Override
                public void imePrev(String id) {
                    ImeState.imePrev(id); // persisted (device-protected) unless empty
                    Log.i(TAG, "ime_prev stored" + (id == null || id.isEmpty() ? " (empty)" : ""));
                }

                @Override
                public void ime(int display, String op, String text) {
                    LanternIme ime = ImeState.ime;
                    if (ime == null) {
                        // Not the selected keyboard (or not created yet): nowhere to type.
                        Log.i(TAG, ImeOps.logLine(op, false, text)); // never the text itself
                        return;
                    }
                    ime.post(display, op, text);
                }

                @Override
                public void sync() {
                    if (cat == null) {
                        Log.w(TAG, "sync ignored: no catalog");
                        return;
                    }
                    Log.i(TAG, "sync: sending the app catalogue");
                    cat.requestSync();
                }
            });
            Log.i(TAG, "link closed by the helper");
        } catch (IOException | RuntimeException e) {
            if (!stopped) {
                Log.w(TAG, "link failed: " + e);
            }
        } finally {
            if (catalog != null) {
                catalog.close();
            }
            client = null;
            stop();
            synchronized (Link.class) {
                if (running == this) {
                    running = null;
                    if (linked) {
                        // Only the current link's loss counts: a link replaced by a newer one
                        // must not arm the watchdog against the new session's keyboard.
                        ImeState.WATCHDOG.linkLost(SystemClock.elapsedRealtime(), ImeState.restoreTarget());
                        Log.i(TAG, "link lost; the keyboard is given back in "
                            + ImeWatchdog.DEFAULT_GRACE_MS + " ms unless the helper relinks");
                    }
                }
            }
        }
    }
}
