package dev.lantern.companion;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.pm.LauncherActivityInfo;
import android.content.pm.LauncherApps;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.UserHandle;
import android.util.Log;

import java.io.ByteArrayOutputStream;
import java.util.ArrayList;
import java.util.List;

/**
 * The device side of the catalogue: {@link LauncherApps} as the {@link AppCatalog.Source}, its
 * callback and the profile broadcasts as live updates. Everything (sync, callbacks, broadcasts)
 * runs on one {@link HandlerThread} with a running Looper, so updates never interleave with a
 * full sync. One instance per link; {@link #close} when the link ends.
 */
final class LauncherCatalog implements AppCatalog.Source {
    /** Icon size in pixels (ARGB, PNG). */
    static final int ICON_PX = 192;

    /** Profile changes that call for a full sync: the managed-profile ones and the general ones (any profile type). */
    private static final String[] PROFILE_ACTIONS = {
        Intent.ACTION_MANAGED_PROFILE_AVAILABLE, Intent.ACTION_MANAGED_PROFILE_UNAVAILABLE,
        Intent.ACTION_MANAGED_PROFILE_ADDED, Intent.ACTION_MANAGED_PROFILE_REMOVED,
        Intent.ACTION_PROFILE_AVAILABLE, Intent.ACTION_PROFILE_UNAVAILABLE,
        Intent.ACTION_PROFILE_ADDED, Intent.ACTION_PROFILE_REMOVED,
    };

    private final Context context;
    private final LauncherApps launcher;
    private final HandlerThread thread = new HandlerThread("lantern-catalog");
    private final Handler handler;
    private final AppCatalog catalog;
    private final LauncherApps.Callback callback = new LauncherApps.Callback() {
        @Override
        public void onPackageRemoved(String pkg, UserHandle user) {
            catalog.removed(pkg, id(user));
        }

        @Override
        public void onPackageAdded(String pkg, UserHandle user) {
            catalog.changed(pkg, id(user));
        }

        @Override
        public void onPackageChanged(String pkg, UserHandle user) {
            catalog.changed(pkg, id(user));
        }

        @Override
        public void onPackagesAvailable(String[] pkgs, UserHandle user, boolean replacing) {
            for (String p : pkgs) {
                catalog.changed(p, id(user));
            }
        }

        @Override
        public void onPackagesUnavailable(String[] pkgs, UserHandle user, boolean replacing) {
            if (replacing) {
                return; // an update: onPackageChanged/Available follows
            }
            for (String p : pkgs) {
                catalog.removed(p, id(user));
            }
        }
    };
    private final BroadcastReceiver profiles = new BroadcastReceiver() {
        @Override
        public void onReceive(Context c, Intent intent) {
            Log.i(Link.TAG, "catalog: " + intent.getAction() + ", full sync");
            catalog.profilesChanged();
        }
    };

    LauncherCatalog(Context context, AppCatalog.Sink sink) {
        this.context = context;
        launcher = context.getSystemService(LauncherApps.class);
        thread.start();
        handler = new Handler(thread.getLooper());
        catalog = new AppCatalog(this, sink, msg -> Log.i(Link.TAG, msg));
        // Registered up front; AppCatalog drops what arrives before the first sync.
        launcher.registerCallback(callback, handler);
        IntentFilter f = new IntentFilter();
        for (String a : PROFILE_ACTIONS) {
            f.addAction(a);
        }
        context.registerReceiver(profiles, f, null, handler, Context.RECEIVER_NOT_EXPORTED);
    }

    /** The helper's {@code sync}: a full catalogue, on the catalogue thread. */
    void requestSync() {
        handler.post(catalog::sync);
    }

    void close() {
        try {
            launcher.unregisterCallback(callback);
        } catch (RuntimeException ignored) {
            // already gone
        }
        try {
            context.unregisterReceiver(profiles);
        } catch (RuntimeException ignored) {
            // already gone
        }
        thread.quitSafely();
    }

    @Override
    public List<AppCatalog.Entry> all() {
        List<AppCatalog.Entry> out = new ArrayList<>();
        for (UserHandle h : launcher.getProfiles()) {
            try {
                for (LauncherActivityInfo i : launcher.getActivityList(null, h)) {
                    out.add(entry(i, id(h)));
                }
            } catch (RuntimeException e) {
                Log.w(Link.TAG, "catalog: listing user " + id(h) + " failed: " + e);
            }
        }
        return out;
    }

    @Override
    public List<AppCatalog.Entry> of(String pkg, int user) {
        List<AppCatalog.Entry> out = new ArrayList<>();
        for (UserHandle h : launcher.getProfiles()) {
            if (id(h) != user) {
                continue;
            }
            for (LauncherActivityInfo i : launcher.getActivityList(pkg, h)) {
                out.add(entry(i, user));
            }
        }
        return out;
    }

    private static AppCatalog.Entry entry(LauncherActivityInfo i, int user) {
        CharSequence label = i.getLabel();
        return new AppCatalog.Entry(i.getComponentName().getPackageName(), user,
            label == null ? "" : label.toString(), () -> render(i));
    }

    /** The badged icon drawn onto {@value #ICON_PX}x{@value #ICON_PX} ARGB, as PNG. */
    private static byte[] render(LauncherActivityInfo i) {
        Drawable d = i.getBadgedIcon(0);
        if (d == null) {
            return null;
        }
        Bitmap b = Bitmap.createBitmap(ICON_PX, ICON_PX, Bitmap.Config.ARGB_8888);
        try {
            Canvas c = new Canvas(b);
            d.setBounds(0, 0, ICON_PX, ICON_PX);
            d.draw(c);
            ByteArrayOutputStream png = new ByteArrayOutputStream();
            b.compress(Bitmap.CompressFormat.PNG, 100, png);
            return png.toByteArray();
        } finally {
            b.recycle();
        }
    }

    /**
     * {@code UserHandle.getIdentifier()}: hidden from apps, so read reflectively, falling back to
     * {@code hashCode()}, which is the id on AOSP.
     */
    static int id(UserHandle h) {
        try {
            return (int) UserHandle.class.getMethod("getIdentifier").invoke(h);
        } catch (ReflectiveOperationException | RuntimeException e) {
            return h.hashCode();
        }
    }
}
