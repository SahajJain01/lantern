package dev.lantern.companion;

import android.inputmethodservice.InputMethodService;

/** Lantern's hidden IME: commits PC text into the focused field. Stub; Task 7 fills it in. */
public final class LanternIme extends InputMethodService {
    /** Never show a soft keyboard on the phone: the text comes from the PC. */
    @Override
    public boolean onEvaluateInputViewShown() {
        return false;
    }
}
