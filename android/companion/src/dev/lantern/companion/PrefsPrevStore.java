package dev.lantern.companion;

import android.content.Context;
import android.content.SharedPreferences;
import android.util.Log;

/**
 * {@link PrevStore} in device-protected storage ({@link Context#createDeviceProtectedStorageContext}),
 * so the keyboard to give back is readable right after a reboot, before the first unlock. Writes
 * with {@code commit()}: the value must be on disk before the phone can die.
 */
final class PrefsPrevStore implements PrevStore {
    private static final String FILE = "lantern-ime";
    private static final String KEY = "ime_prev";

    private final SharedPreferences prefs;

    PrefsPrevStore(Context context) {
        prefs = context.createDeviceProtectedStorageContext().getSharedPreferences(FILE, Context.MODE_PRIVATE);
    }

    /** Swaps the device store into {@link ImeState} (once; never throws). */
    static void install(Context context) {
        if (!(ImeState.store instanceof ImeState.MemoryStore)) {
            return;
        }
        try {
            Context app = context.getApplicationContext();
            ImeState.useStore(new PrefsPrevStore(app != null ? app : context));
        } catch (RuntimeException e) {
            Log.w(Link.TAG, "ime_prev store unavailable: " + e);
        }
    }

    @Override
    public String get() {
        return prefs.getString(KEY, null);
    }

    @Override
    public void put(String id) {
        prefs.edit().putString(KEY, id).commit();
    }

    @Override
    public void clear() {
        prefs.edit().remove(KEY).commit();
    }
}
