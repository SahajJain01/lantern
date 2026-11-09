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

    /**
     * Whether an {@code ime} typed in the window on display {@code want} may be applied by the IME
     * on display {@code have}. Either one unknown ({@code -1}) lets it through: typing never breaks
     * because a display could not be read.
     */
    static boolean displayMatches(int want, int have) {
        return want < 0 || have < 0 || want == have;
    }

    /** The log line of an {@code ime} dropped for another display: op, length and reason, never the text. */
    static String droppedLine(String op, String text, int want, int have) {
        return "ime op=" + op + " dropped len=" + (text == null ? 0 : text.length()) + ": typed for display "
            + want + ", the keyboard is on display " + have;
    }

    /** {@code ime op=<op> ok=<bool> len=<n>}: the log line of an applied {@code ime} (never its text). */
    static String logLine(String op, boolean ok, String text) {
        return "ime op=" + op + " ok=" + ok + " len=" + (text == null ? 0 : text.length());
    }
}
