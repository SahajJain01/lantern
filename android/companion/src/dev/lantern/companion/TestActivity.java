package dev.lantern.companion;

import android.app.Activity;
import android.os.Bundle;
import android.text.Editable;
import android.text.TextWatcher;
import android.util.Log;
import android.widget.EditText;

/**
 * Test-only text readback screen (disabled in the manifest; device tests enable it only while they
 * run): one EditText in the top 20% of the screen, focused on start. Every change logs
 * {@code text=<full content>} with tag {@value #TAG}.
 */
public final class TestActivity extends Activity {
    static final String TAG = "LanternTest";

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        setContentView(R.layout.test);
        EditText text = findViewById(R.id.text);
        text.addTextChangedListener(new TextWatcher() {
            @Override
            public void beforeTextChanged(CharSequence s, int start, int count, int after) {
            }

            @Override
            public void onTextChanged(CharSequence s, int start, int before, int count) {
            }

            @Override
            public void afterTextChanged(Editable s) {
                Log.i(TAG, "text=" + s);
            }
        });
        text.requestFocus();
    }
}
