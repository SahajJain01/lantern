package dev.lantern.helper;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;

/**
 * {@link SettingsBackend} over the {@code settings} command (the shell uid may write global).
 * Absence is detected via {@code settings list} membership, since {@code settings get} prints
 * {@code null} for both an absent key and a literal "null" value.
 */
final class ShellSettings implements SettingsBackend {
    @Override
    public String get(String ns, String key) {
        String prefix = key + "=";
        boolean present = false;
        for (String line : run("settings", "list", ns).split("\n")) {
            if (line.startsWith(prefix)) {
                present = true;
                break;
            }
        }
        if (!present) {
            return null;
        }
        String v = run("settings", "get", ns, key);
        return v.endsWith("\n") ? v.substring(0, v.length() - 1) : v;
    }

    @Override
    public void put(String ns, String key, String v) {
        if (v == null) {
            throw new IllegalArgumentException("put null; use delete");
        }
        run("settings", "put", ns, key, v);
    }

    @Override
    public void delete(String ns, String key) {
        run("settings", "delete", ns, key);
    }

    private static String run(String... cmd) {
        try {
            Process p = new ProcessBuilder(cmd).redirectErrorStream(true).start();
            p.getOutputStream().close();
            String out = readAll(p.getInputStream());
            int code = p.waitFor();
            if (code != 0) {
                throw new IllegalStateException(String.join(" ", cmd) + " exited " + code + ": " + out.trim());
            }
            return out;
        } catch (IOException e) {
            throw new IllegalStateException(String.join(" ", cmd), e);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new IllegalStateException(String.join(" ", cmd) + " interrupted", e);
        }
    }

    private static String readAll(InputStream in) throws IOException {
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        byte[] b = new byte[4096];
        int n;
        while ((n = in.read(b)) != -1) {
            buf.write(b, 0, n);
        }
        return buf.toString(StandardCharsets.UTF_8.name());
    }
}
