package dev.lantern.companion;

/**
 * {@code ime} op dispatch (pure, so it runs on the host JVM): {@link LanternIme} adapts its
 * {@code InputConnection} to a {@link Target}. Never logs text: {@link #logLine} carries the op,
 * the result and the text's length only.
 */
final class ImeOps {
    private ImeOps() {
    }

    /** The focused editor (on the device: the current {@code InputConnection}). */
    interface Target {
        /** {@code commitText(text, 1)}. */
        boolean commit(String text);

        /** {@code setComposingText(text, 1)}; {@code ""} clears the composing text. */
        boolean compose(String text);

        /** {@code finishComposingText()}. */
        boolean finish();
    }

    /** Applies {@code op} to {@code t}. False for no editor ({@code t} null), an unknown op, or a refused call. */
    static boolean apply(Target t, String op, String text) {
        if (t == null) {
            return false;
        }
        String s = text == null ? "" : text;
        switch (op) {
            case "commit":
                return t.commit(s);
            case "compose":
                return t.compose(s);
            case "finish":
                return t.finish();
            default:
                return false;
        }
    }

    /** {@code ime op=<op> ok=<bool> len=<n>}: the only log line an {@code ime} gets. */
    static String logLine(String op, boolean ok, String text) {
        return "ime op=" + op + " ok=" + ok + " len=" + (text == null ? 0 : text.length());
    }
}
