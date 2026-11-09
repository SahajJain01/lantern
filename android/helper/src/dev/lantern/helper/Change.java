package dev.lantern.helper;

import java.util.function.UnaryOperator;

/**
 * One phone setting a {@link Session} changes. {@link #next} computes the value to write from the
 * current one (null = absent); {@link #apply} writes it. Journaled before it is applied.
 */
public final class Change {
    public final String ns;
    public final String key;
    private final UnaryOperator<String> next;
    private final boolean ime;

    private Change(String ns, String key, UnaryOperator<String> next, boolean ime) {
        this.ns = ns;
        this.key = key;
        this.next = next;
        this.ime = ime;
    }

    /** A plain setting; {@code next} gets the current value, which may be null. */
    public static Change setting(String ns, String key, UnaryOperator<String> next) {
        return new Change(ns, key, next, false);
    }

    /** Selects input method {@code id} ({@code secure/default_input_method}) via {@code selectIme}. */
    public static Change ime(String id) {
        return new Change(Journal.IME_NS, Journal.IME_KEY, current -> id, true);
    }

    public String next(String current) {
        return next.apply(current);
    }

    /** Writes {@code value}; null deletes the key. */
    public void apply(SettingsBackend s, String value) {
        if (value == null) {
            s.delete(ns, key);
        } else if (ime) {
            s.selectIme(value);
        } else {
            s.put(ns, key, value);
        }
    }

    @Override
    public String toString() {
        return (ime ? "ime " : "setting ") + ns + "/" + key;
    }
}
