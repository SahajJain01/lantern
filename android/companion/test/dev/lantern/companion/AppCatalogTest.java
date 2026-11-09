package dev.lantern.companion;

import dev.lantern.helper.Msg;
import dev.lantern.helper.Proto;

import java.util.ArrayList;
import java.util.Base64;
import java.util.Collections;
import java.util.List;

/**
 * Host-JVM tests for {@link AppCatalog}'s pure core: icon encoding, the {@code app} message, and the
 * full sync / live update rules over a fake {@link AppCatalog.Source}. Android classes must never
 * be loaded here.
 */
public final class AppCatalogTest {
    /** A launcher whose apps a test edits; counts icon renders. */
    static final class FakeSource implements AppCatalog.Source {
        final List<AppCatalog.Entry> apps = new ArrayList<>();
        int renders;

        FakeSource add(String pkg, int user, String label) {
            byte[] png = (pkg + ":" + user).getBytes(java.nio.charset.StandardCharsets.UTF_8);
            apps.add(new AppCatalog.Entry(pkg, user, label, () -> {
                renders++;
                return png;
            }));
            return this;
        }

        @Override
        public List<AppCatalog.Entry> all() {
            return new ArrayList<>(apps);
        }

        @Override
        public List<AppCatalog.Entry> of(String pkg, int user) {
            List<AppCatalog.Entry> out = new ArrayList<>();
            for (AppCatalog.Entry e : apps) {
                if (e.pkg.equals(pkg) && e.user == user) {
                    out.add(e);
                }
            }
            return out;
        }
    }

    static final class Rig {
        final FakeSource source = new FakeSource();
        final List<Msg> sent = Collections.synchronizedList(new ArrayList<>());
        final List<String> logs = new ArrayList<>();
        volatile boolean linkUp = true;
        final AppCatalog catalog = new AppCatalog(source, m -> {
            if (!linkUp) {
                return false;
            }
            sent.add(m);
            return true;
        }, logs::add);

        /** "t pkg:user" per message ("apps_end N" for the end). */
        List<String> summary() {
            List<String> out = new ArrayList<>();
            synchronized (sent) {
                for (Msg m : sent) {
                    try {
                        if (m.t.equals("apps_end")) {
                            out.add("apps_end " + m.num("count"));
                        } else {
                            out.add(m.t + " " + m.str("pkg") + ":" + m.num("user"));
                        }
                    } catch (java.io.IOException e) {
                        throw new AssertionError(e);
                    }
                }
            }
            return out;
        }
    }

    static void iconOverLimitIsEmpty() {
        eq("", AppCatalog.encodeIcon(new byte[Proto.MAX_ICON + 1]), "over MAX_ICON");
        eq("", AppCatalog.encodeIcon(null), "no icon");
        byte[] at = new byte[Proto.MAX_ICON];
        at[0] = (byte) 0x89;
        String b64 = AppCatalog.encodeIcon(at);
        check(!b64.isEmpty(), "exactly MAX_ICON is kept");
        check(!b64.contains("\n") && !b64.contains("\r"), "NO_WRAP");
        check(java.util.Arrays.equals(at, Base64.getDecoder().decode(b64)), "round trip");
        eq("iVBORw==", AppCatalog.encodeIcon(new byte[] {(byte) 0x89, 'P', 'N', 'G'}), "plain base64");
    }

    static void appMsgFields() throws Exception {
        Msg m = AppCatalog.appMsg("com.android.settings", 10, "Settings", "iVBO");
        eq("app", m.t, "t");
        eq("[pkg, user, label, icon]", m.fields().keySet().toString(), "fields, in order, no work");
        eq("com.android.settings", m.str("pkg"), "pkg");
        eq(10L, m.num("user"), "user");
        eq("Settings", m.str("label"), "label");
        eq("iVBO", m.str("icon"), "icon");
        eq("", AppCatalog.appMsg("x", 0, null, "").str("label"), "null label is empty");
    }

    static void updatesBeforeSyncDropped() {
        Rig r = new Rig();
        r.source.add("org.a", 0, "A");
        r.catalog.changed("org.a", 0);
        r.catalog.removed("org.b", 0);
        r.catalog.profilesChanged();
        eq("[]", r.summary().toString(), "nothing sent before the first sync");
        eq(0, r.source.renders, "no icon rendered");
        check(!r.catalog.synced(), "not synced");
    }

    static void syncSendsFirstPerPackageThenCount() {
        Rig r = new Rig();
        r.source.add("org.a", 0, "A").add("org.a", 0, "A second activity").add("org.b", 0, "B")
            .add("org.a", 10, "A (work)");
        r.catalog.sync();
        eq("[app org.a:0, app org.b:0, app org.a:10, apps_end 3]", r.summary().toString(), "full sync");
        eq(3, r.source.renders, "the duplicate activity's icon is never rendered");
        check(r.catalog.synced(), "synced");
        try {
            eq("A", r.sent.get(0).str("label"), "first activity's label");
            eq(AppCatalog.encodeIcon("org.a:0".getBytes(java.nio.charset.StandardCharsets.UTF_8)),
                r.sent.get(0).str("icon"), "icon");
        } catch (java.io.IOException e) {
            throw new AssertionError(e);
        }
    }

    static void liveUpdatesAfterSync() {
        Rig r = new Rig();
        r.source.add("org.a", 0, "A");
        r.catalog.sync();
        r.sent.clear();
        r.source.add("org.new", 10, "New");
        r.catalog.changed("org.new", 10);
        r.catalog.removed("org.a", 0);
        r.catalog.changed("org.gone", 0); // changed but no launcher activity left: removed
        r.catalog.profilesChanged();
        eq("[app org.new:10, app_removed org.a:0, app_removed org.gone:0, app org.a:0, app org.new:10, apps_end 2]",
            r.summary().toString(), "updates, then a full sync for the profile change");
    }

    static void syncStopsWhenTheLinkFails() {
        Rig r = new Rig();
        r.source.add("org.a", 0, "A").add("org.b", 0, "B");
        r.linkUp = false;
        r.catalog.sync();
        eq("[]", r.summary().toString(), "nothing");
        eq(1, r.source.renders, "stopped after the first failed send");
    }

    static void brokenIconIsEmpty() throws Exception {
        Rig r = new Rig();
        r.source.apps.add(new AppCatalog.Entry("org.a", 0, "A", () -> {
            throw new IllegalStateException("no drawable");
        }));
        r.catalog.sync();
        eq("[app org.a:0, apps_end 1]", r.summary().toString(), "still listed");
        eq("", r.sent.get(0).str("icon"), "empty icon");
    }

    static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"iconOverLimitIsEmpty", (Case) AppCatalogTest::iconOverLimitIsEmpty},
            {"appMsgFields", (Case) AppCatalogTest::appMsgFields},
            {"updatesBeforeSyncDropped", (Case) AppCatalogTest::updatesBeforeSyncDropped},
            {"syncSendsFirstPerPackageThenCount", (Case) AppCatalogTest::syncSendsFirstPerPackageThenCount},
            {"liveUpdatesAfterSync", (Case) AppCatalogTest::liveUpdatesAfterSync},
            {"syncStopsWhenTheLinkFails", (Case) AppCatalogTest::syncStopsWhenTheLinkFails},
            {"brokenIconIsEmpty", (Case) AppCatalogTest::brokenIconIsEmpty},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   AppCatalogTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL AppCatalogTest." + c[0] + ": " + t);
            }
        }
        System.out.println("AppCatalogTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
