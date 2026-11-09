package dev.lantern.companion;

import dev.lantern.helper.Msg;
import dev.lantern.helper.Proto;

import java.util.Base64;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.function.Consumer;
import java.util.function.Supplier;

/**
 * The app catalogue's rules (docs/protocol.md, "Catalogue"), pure Java so host tests can run it;
 * {@link LauncherCatalog} is the device {@link Source} and calls it on one thread.
 *
 * <ul>
 * <li>{@link #sync}: one {@code app} per package+user (its first launcher activity), then
 * {@code apps_end{count}}. Asked for by the helper's {@code sync}.</li>
 * <li>{@link #changed}/{@link #removed}: live updates, sent only once a sync has run (until then
 * they are dropped: the sync covers them).</li>
 * <li>{@link #profilesChanged}: a profile became (un)available, was added or removed: a full sync,
 * again only after the first one.</li>
 * </ul>
 *
 * <p>Never logs icons or whole messages: pkg, user, label and icon length at most.
 */
final class AppCatalog {
    /** One launcher activity. {@code png} renders the badged icon (lazily: only the first per package+user). */
    static final class Entry {
        final String pkg;
        final int user;
        final String label;
        final Supplier<byte[]> png;

        Entry(String pkg, int user, String label, Supplier<byte[]> png) {
            this.pkg = pkg;
            this.user = user;
            this.label = label;
            this.png = png;
        }
    }

    /** The launcher's activities. */
    interface Source {
        /** Every launcher activity of every profile, profile by profile. */
        List<Entry> all();

        /** {@code pkg}'s launcher activities in {@code user} (empty when it has none left). */
        List<Entry> of(String pkg, int user);
    }

    /** The link to the helper; false when the write failed (link gone). */
    interface Sink {
        boolean send(Msg m);
    }

    private final Source source;
    private final Sink sink;
    private final Consumer<String> log;
    private boolean synced;

    AppCatalog(Source source, Sink sink, Consumer<String> log) {
        this.source = source;
        this.sink = sink;
        this.log = log;
    }

    synchronized boolean synced() {
        return synced;
    }

    /** Sends the full catalogue. Stops at the first failed send (no {@code apps_end}: partial). */
    synchronized void sync() {
        synced = true;
        List<Entry> all;
        try {
            all = source.all();
        } catch (RuntimeException e) {
            log.accept("catalog: listing apps failed: " + e);
            return;
        }
        Set<String> seen = new HashSet<>();
        long count = 0;
        for (Entry e : all) {
            if (!seen.add(e.pkg + ":" + e.user)) {
                continue; // only the first launcher activity per package+user
            }
            if (!sink.send(appMsg(e.pkg, e.user, e.label, icon(e)))) {
                log.accept("catalog: sync stopped after " + count + " apps (link gone)");
                return;
            }
            count++;
        }
        sink.send(Msg.of("apps_end").put("count", count));
        log.accept("catalog: sync sent apps_end count=" + count);
    }

    /** {@code pkg} was added or changed in {@code user}: its {@code app}, or {@code app_removed} if it has no launcher activity now. */
    synchronized void changed(String pkg, int user) {
        if (!synced) {
            return;
        }
        List<Entry> now;
        try {
            now = source.of(pkg, user);
        } catch (RuntimeException e) {
            log.accept("catalog: listing " + pkg + " user=" + user + " failed: " + e);
            return;
        }
        if (now.isEmpty()) {
            sink.send(Msg.of("app_removed").put("pkg", pkg).put("user", user));
            return;
        }
        Entry e = now.get(0);
        String icon = icon(e);
        sink.send(appMsg(e.pkg, e.user, e.label, icon));
        log.accept("catalog: app " + pkg + " user=" + user + " icon=" + icon.length());
    }

    synchronized void removed(String pkg, int user) {
        if (!synced) {
            return;
        }
        sink.send(Msg.of("app_removed").put("pkg", pkg).put("user", user));
        log.accept("catalog: app_removed " + pkg + " user=" + user);
    }

    synchronized void profilesChanged() {
        if (synced) {
            sync();
        }
    }

    private String icon(Entry e) {
        try {
            return encodeIcon(e.png.get());
        } catch (RuntimeException ex) {
            log.accept("catalog: icon of " + e.pkg + " user=" + e.user + " failed: " + ex);
            return "";
        }
    }

    /** Base64 (no line breaks) of {@code png}; {@code ""} when there is none or it is over {@link Proto#MAX_ICON}. */
    static String encodeIcon(byte[] png) {
        if (png == null || png.length > Proto.MAX_ICON) {
            return "";
        }
        return Base64.getEncoder().encodeToString(png);
    }

    /** {@code app{pkg, user, label, icon}}; {@code work} is the helper's to add. */
    static Msg appMsg(String pkg, int user, String label, String icon) {
        return Msg.of("app").put("pkg", pkg).put("user", user).put("label", label == null ? "" : label)
            .put("icon", icon == null ? "" : icon);
    }
}
