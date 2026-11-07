package dev.lantern.probe;
import android.util.Log;
import org.json.JSONObject;
final class ProbeLog {
    static void event(String name, Object... fields) {
        StringBuilder b = new StringBuilder("event=").append(name);
        b.append(" elapsed_ms=").append(android.os.SystemClock.elapsedRealtime());
        for(int i=0;i<fields.length;i+=2) {
            Object v=fields[i+1];
            b.append(' ').append(fields[i]).append('=').append(v instanceof Number || v instanceof Boolean ? v.toString() : JSONObject.quote(String.valueOf(v)));
        }
        Log.i("LanternProbe", b.toString());
    }
}
