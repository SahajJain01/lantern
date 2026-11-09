package dev.lantern.companion;

import android.content.BroadcastReceiver;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.util.Log;

/**
 * Shell-only commands (the manifest requires {@code android.permission.DUMP}). The link itself goes
 * through {@link LinkProvider}; this receiver only takes
 * {@code am broadcast --user 0 -n dev.lantern.companion/.LinkReceiver -a
 * dev.lantern.companion.TEST_ACTIVITY --es enabled true|false}: enable the test-only
 * {@link TestActivity}, or return it to its manifest state (disabled). Android 15 refuses
 * {@code pm enable} of a component from the shell unless the package is testOnly, so device tests
 * ask the companion to toggle its own component. Result code 1 = done, 2 = malformed.
 */
public final class LinkReceiver extends BroadcastReceiver {
    static final String ACTION_TEST_ACTIVITY = "dev.lantern.companion.TEST_ACTIVITY";

    @Override
    public void onReceive(Context context, Intent intent) {
        if (!ACTION_TEST_ACTIVITY.equals(intent.getAction())) {
            return;
        }
        Boolean on = LinkClient.testActivityState(intent.getStringExtra("enabled"));
        if (on == null) {
            Log.w(Link.TAG, "test-activity broadcast without enabled=true|false, ignored");
            setResultCode(2);
            return;
        }
        context.getPackageManager().setComponentEnabledSetting(new ComponentName(context, TestActivity.class),
            on ? PackageManager.COMPONENT_ENABLED_STATE_ENABLED : PackageManager.COMPONENT_ENABLED_STATE_DEFAULT,
            PackageManager.DONT_KILL_APP);
        Log.i(Link.TAG, "test activity " + (on ? "enabled" : "back to disabled"));
        setResultCode(1);
    }
}
