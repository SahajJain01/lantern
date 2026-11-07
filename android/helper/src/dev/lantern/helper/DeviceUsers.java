package dev.lantern.helper;

import android.content.Intent;
import android.os.IBinder;

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.List;

/**
 * {@link UserLaunch.Backend} on the phone: IUserManager, IActivityManager and IPackageManager over
 * binder (hidden APIs, by reflection), each asked for the target user. Never load from host tests.
 */
final class DeviceUsers implements UserLaunch.Backend {
    /** UserManager.DISALLOW_DEBUGGING_FEATURES. */
    static final String NO_DEBUGGING = "no_debugging_features";

    private final Object um;
    private final Object am;
    private final Object pm;

    DeviceUsers() throws ReflectiveOperationException {
        IBinder user = (IBinder) Class.forName("android.os.ServiceManager")
            .getMethod("getService", String.class).invoke(null, "user");
        um = Class.forName("android.os.IUserManager$Stub").getMethod("asInterface", IBinder.class).invoke(null, user);
        am = Class.forName("android.app.ActivityManager").getMethod("getService").invoke(null);
        pm = Class.forName("android.app.ActivityThread").getMethod("getPackageManager").invoke(null);
    }

    @Override
    public int[] users() {
        // getUsers(excludePartial, excludeDying, excludePreCreated) -> List<UserInfo>
        List<?> infos = (List<?>) call(um, "getUsers", true, true, true);
        int[] ids = new int[infos.size()];
        for (int i = 0; i < ids.length; i++) {
            try {
                ids[i] = infos.get(i).getClass().getField("id").getInt(infos.get(i));
            } catch (ReflectiveOperationException e) {
                throw new IllegalStateException("UserInfo.id", e);
            }
        }
        return ids;
    }

    @Override
    public boolean running(int user) {
        return (boolean) call(am, "isUserRunning", user, 0);
    }

    @Override
    public boolean debuggingBlocked(int user) {
        return (boolean) call(um, "hasUserRestriction", NO_DEBUGGING, user);
    }

    @Override
    public boolean installed(String pkg, int user) {
        // getApplicationInfo(pkg, flags, userId): null when the package isn't installed for that user.
        return call(pm, "getApplicationInfo", pkg, 0L, user) != null;
    }

    @Override
    public String launcherActivity(String pkg, int user) {
        // Same order as PackageManager.getLaunchIntentForPackage: LAUNCHER, then INFO.
        for (String category : new String[] {Intent.CATEGORY_LAUNCHER, Intent.CATEGORY_INFO}) {
            Intent intent = new Intent(Intent.ACTION_MAIN).addCategory(category).setPackage(pkg);
            Object slice = call(pm, "queryIntentActivities", intent, null, 0L, user);
            List<?> list = slice == null ? null : (List<?>) call(slice, "getList");
            if (list != null && !list.isEmpty()) {
                android.content.pm.ResolveInfo ri = (android.content.pm.ResolveInfo) list.get(0);
                return ri.activityInfo.name;
            }
        }
        return null;
    }

    /**
     * Calls the binder method {@code name} with {@code args}, matching on name and arity (flags are
     * {@code int} on some releases and {@code long} on others). Binder exceptions are rethrown as is.
     */
    static Object call(Object target, String name, Object... args) {
        for (Method m : target.getClass().getMethods()) {
            Class<?>[] types = m.getParameterTypes();
            if (!m.getName().equals(name) || types.length != args.length) {
                continue;
            }
            Object[] a = new Object[args.length];
            boolean ok = true;
            for (int i = 0; i < args.length && ok; i++) {
                a[i] = adapt(types[i], args[i]);
                ok = a[i] != NO;
            }
            if (!ok) {
                continue;
            }
            try {
                return m.invoke(target, a);
            } catch (InvocationTargetException e) {
                Throwable c = e.getCause();
                if (c instanceof RuntimeException) {
                    throw (RuntimeException) c;
                }
                throw new IllegalStateException(name, c);
            } catch (IllegalAccessException e) {
                throw new IllegalStateException(name, e);
            }
        }
        throw new IllegalStateException("no " + target.getClass().getName() + "." + name + "/" + args.length);
    }

    private static final Object NO = new Object();

    private static Object adapt(Class<?> type, Object arg) {
        if (type == long.class && arg instanceof Number) {
            return ((Number) arg).longValue();
        }
        if (type == int.class && arg instanceof Number) {
            return ((Number) arg).intValue();
        }
        if (type == boolean.class) {
            return arg instanceof Boolean ? arg : NO;
        }
        if (type.isPrimitive()) {
            return NO;
        }
        return arg == null || type.isInstance(arg) ? arg : NO;
    }
}
