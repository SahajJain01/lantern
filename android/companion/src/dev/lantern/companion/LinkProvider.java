package dev.lantern.companion;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.net.Uri;
import android.os.Binder;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.util.Log;

/**
 * How the helper links to the companion: {@code call("link", <link token>, {in, out})} on
 * {@code content://dev.lantern.companion.link}, from the shell only (DUMP-protected and checked by
 * calling uid). {@code in} is the read end of the helper-to-companion pipe and {@code out} the
 * write end of the companion-to-helper pipe; the link protocol (hello with the token, then frames)
 * runs over them. The helper holds this provider as an external client for the whole session.
 *
 * <p>Pipes, not sockets: SELinux denies an app connecting to the helper's Unix socket, and the
 * network policy blocks the companion's loopback TCP.
 */
public final class LinkProvider extends ContentProvider {
    static final String METHOD_LINK = "link";

    @Override
    public boolean onCreate() {
        return true;
    }

    @Override
    public Bundle call(String method, String arg, Bundle extras) {
        int uid = Binder.getCallingUid();
        if (!LinkClient.callerAllowed(uid)) {
            throw new SecurityException("only the shell may link the companion (uid " + uid + ")");
        }
        if (!METHOD_LINK.equals(method)) {
            throw new IllegalArgumentException("unknown method " + method);
        }
        ParcelFileDescriptor in = extras == null ? null : extras.getParcelable("in", ParcelFileDescriptor.class);
        ParcelFileDescriptor out = extras == null ? null : extras.getParcelable("out", ParcelFileDescriptor.class);
        if (!LinkClient.validToken(arg) || in == null || out == null) {
            closeQuietly(in);
            closeQuietly(out);
            Log.w(Link.TAG, "link call with a malformed token or missing pipes, refused");
            Bundle r = new Bundle();
            r.putBoolean("ok", false);
            return r;
        }
        Link.start(new ParcelFileDescriptor.AutoCloseInputStream(in),
            new ParcelFileDescriptor.AutoCloseOutputStream(out), arg);
        Bundle r = new Bundle();
        r.putBoolean("ok", true);
        return r;
    }

    private static void closeQuietly(ParcelFileDescriptor p) {
        if (p != null) {
            try {
                p.close();
            } catch (java.io.IOException ignored) {
                // dropped anyway
            }
        }
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] selectionArgs, String sortOrder) {
        return null;
    }

    @Override
    public String getType(Uri uri) {
        return null;
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        return null;
    }

    @Override
    public int delete(Uri uri, String selection, String[] selectionArgs) {
        return 0;
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] selectionArgs) {
        return 0;
    }
}
