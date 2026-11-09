package dev.lantern.helper;

import android.os.Binder;
import android.os.Bundle;
import android.os.IBinder;
import android.os.ParcelFileDescriptor;

import com.genymobile.scrcpy.FakeContext;

import java.io.Closeable;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.security.SecureRandom;

/**
 * Device side of {@link CompanionLink}: hands a pipe pair to the companion's shell-only provider
 * ({@code content://dev.lantern.companion.link}, method {@code link}, arg = link token, extras
 * {@code in}/{@code out}). The helper holds the provider as an external client until {@link
 * Handoff#close}, which also keeps the companion's process out of the cached (frozen,
 * network-blocked) state while the session runs. Device-only.
 */
final class DeviceCompanion {
    static final String PKG = "dev.lantern.companion";
    static final String AUTHORITY = PKG + ".link";
    static final String METHOD_LINK = "link";

    private DeviceCompanion() {
    }

    /** {@code n} random bytes as lowercase hex. */
    static String hex(int n) {
        byte[] b = new byte[n];
        new SecureRandom().nextBytes(b);
        StringBuilder s = new StringBuilder(n * 2);
        for (byte x : b) {
            s.append(String.format("%02x", x & 0xff));
        }
        return s.toString();
    }

    /** The helper's ends of one pipe pair. */
    private static final class PipeConn implements CompanionLink.Conn {
        private final InputStream in;
        private final OutputStream out;

        PipeConn(ParcelFileDescriptor read, ParcelFileDescriptor write) {
            in = new ParcelFileDescriptor.AutoCloseInputStream(read);
            out = new ParcelFileDescriptor.AutoCloseOutputStream(write);
        }

        @Override
        public InputStream in() {
            return in;
        }

        @Override
        public OutputStream out() {
            return out;
        }

        @Override
        public void close() throws IOException {
            try {
                out.close();
            } finally {
                in.close();
            }
        }
    }

    /** Each {@code link} makes a pipe pair, offers the helper's ends to the listener and calls the provider. */
    static final class Handoff implements CompanionLink.Broadcaster, Closeable {
        private final HandoffListener listener;
        private final IBinder clientToken = new Binder();
        private Object activityManager;
        private Object provider; // IContentProvider, held until close()

        Handoff(HandoffListener listener) {
            this.listener = listener;
        }

        @Override
        public synchronized void link(String socket, String token) throws IOException {
            Object p = provider();
            ParcelFileDescriptor[] toCompanion = ParcelFileDescriptor.createPipe();
            ParcelFileDescriptor[] fromCompanion = ParcelFileDescriptor.createPipe();
            listener.offer(new PipeConn(fromCompanion[0], toCompanion[1]));
            Bundle extras = new Bundle();
            extras.putParcelable("in", toCompanion[0]);
            extras.putParcelable("out", fromCompanion[1]);
            try {
                Method call = p.getClass().getMethod("call", android.content.AttributionSource.class,
                    String.class, String.class, String.class, Bundle.class);
                Bundle r = (Bundle) call.invoke(p, FakeContext.get().getAttributionSource(), AUTHORITY,
                    METHOD_LINK, token, extras);
                if (r == null || !r.getBoolean("ok")) {
                    throw new IOException("companion refused the link");
                }
            } catch (ReflectiveOperationException e) {
                Throwable cause = e.getCause() != null ? e.getCause() : e;
                throw new IOException("companion link call failed: " + cause, cause);
            } finally {
                // The companion has its own copies now (or never will): drop ours.
                toCompanion[0].close();
                fromCompanion[1].close();
            }
        }

        private Object provider() throws IOException {
            if (provider != null) {
                return provider;
            }
            try {
                Class<?> amClass = Class.forName("android.app.ActivityManager");
                activityManager = amClass.getMethod("getService").invoke(null);
                Method get = activityManager.getClass().getMethod("getContentProviderExternal",
                    String.class, int.class, IBinder.class, String.class);
                Object holder = get.invoke(activityManager, AUTHORITY, 0, clientToken, "lantern");
                if (holder == null) {
                    throw new IOException("companion provider " + AUTHORITY + " not found (companion not installed?)");
                }
                Field f = holder.getClass().getDeclaredField("provider");
                f.setAccessible(true);
                provider = f.get(holder);
                if (provider == null) {
                    throw new IOException("companion provider " + AUTHORITY + " has no binder");
                }
                return provider;
            } catch (ReflectiveOperationException e) {
                Throwable cause = e.getCause() != null ? e.getCause() : e;
                throw new IOException("acquire " + AUTHORITY + ": " + cause, cause);
            }
        }

        /** Releases the external provider reference (idempotent). */
        @Override
        public synchronized void close() {
            if (provider == null) {
                return;
            }
            provider = null;
            try {
                activityManager.getClass().getMethod("removeContentProviderExternal", String.class, IBinder.class)
                    .invoke(activityManager, AUTHORITY, clientToken);
            } catch (ReflectiveOperationException | RuntimeException e) {
                Droid.log("companion: release provider failed: " + e);
            }
        }
    }
}
