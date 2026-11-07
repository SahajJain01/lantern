package dev.lantern.helper;

import android.os.SystemClock;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;

import android.os.IBinder;

import com.genymobile.scrcpy.wrappers.DisplayControl;
import com.genymobile.scrcpy.wrappers.InputManager;
import com.genymobile.scrcpy.wrappers.ServiceManager;
import com.genymobile.scrcpy.wrappers.SurfaceControl;

/** Real {@link Power}: KEYCODE_WAKEUP via InputManager, panel via SurfaceControl power mode. */
final class DevicePower implements Power {
    static final int KEYCODE_WAKEUP = 224;
    private static final int PANEL = 0;
    private static final long WAKE_TIMEOUT_MS = 2000;

    @Override
    public void wake() {
        if (!key(KeyEvent.ACTION_DOWN) || !key(KeyEvent.ACTION_UP)) {
            throw new IllegalStateException("inject KEYCODE_WAKEUP failed");
        }
        // Wait for the wake to land, else a later panel-off can race the screen turning on.
        long deadline = SystemClock.uptimeMillis() + WAKE_TIMEOUT_MS;
        while (!ServiceManager.getPowerManager().isScreenOn(PANEL)) {
            if (SystemClock.uptimeMillis() > deadline) {
                throw new IllegalStateException("screen not on " + WAKE_TIMEOUT_MS + " ms after KEYCODE_WAKEUP");
            }
            SystemClock.sleep(20);
        }
    }

    /**
     * Mirrors scrcpy 5.0.1 {@code Device.setDisplayPower}: SurfaceControl power mode on every
     * physical display (tokens via DisplayControl on API 34+). scrcpy ships its API-35
     * {@code requestDisplayPower} path disabled, and on this phone (SDK 37)
     * {@code DisplayManagerGlobal.requestDisplayPower(int, boolean)} does not exist.
     */
    @Override
    public void setPanel(boolean on) {
        if (on && !ServiceManager.getPowerManager().isScreenOn(PANEL)) {
            // Asleep (e.g. power key during the session): Android already owns the panel; forcing
            // NORMAL would light a panel the system thinks is off. scrcpy's CleanUp skips it too.
            return;
        }
        int mode = on ? SurfaceControl.POWER_MODE_NORMAL : SurfaceControl.POWER_MODE_OFF;
        boolean useDisplayControl = !SurfaceControl.hasGetPhysicalDisplayIdsMethod();
        long[] ids = useDisplayControl ? DisplayControl.getPhysicalDisplayIds() : SurfaceControl.getPhysicalDisplayIds();
        if (ids == null || ids.length == 0) {
            throw new IllegalStateException("no physical display ids");
        }
        boolean ok = true;
        for (long id : ids) {
            IBinder token = useDisplayControl ? DisplayControl.getPhysicalDisplayToken(id)
                    : SurfaceControl.getPhysicalDisplayToken(id);
            ok &= token != null && SurfaceControl.setDisplayPowerMode(token, mode);
        }
        if (!ok) {
            throw new IllegalStateException("setDisplayPowerMode(" + mode + ") failed");
        }
    }

    private static boolean key(int action) {
        long now = SystemClock.uptimeMillis();
        KeyEvent e = new KeyEvent(now, now, action, KEYCODE_WAKEUP, 0, 0, KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0,
                InputDevice.SOURCE_KEYBOARD);
        return ServiceManager.getInputManager().injectInputEvent(e, InputManager.INJECT_INPUT_EVENT_MODE_ASYNC);
    }
}
