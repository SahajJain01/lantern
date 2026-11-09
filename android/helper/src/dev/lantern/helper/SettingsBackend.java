package dev.lantern.helper;

/** Android settings access. {@link #get} returns {@code null} when the key is absent. */
public interface SettingsBackend {
    String get(String ns, String key);

    void put(String ns, String key, String v);

    void delete(String ns, String key);

    /** Selects input method {@code id}; this also sets {@code secure/default_input_method}. */
    void selectIme(String id);
}
