package dev.lantern.probe;
import android.inputmethodservice.InputMethodService;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.provider.Settings;
public class ProbeIme extends InputMethodService {
    static ProbeIme instance;
    final Handler handler=new Handler(Looper.getMainLooper());
    private String previous;
    private long heartbeat;
    private final Runnable watchdog=new Runnable(){public void run(){
        if(previous==null)return;
        if(SystemClock.elapsedRealtime()-heartbeat>5000){
            String current=Settings.Secure.getString(getContentResolver(),Settings.Secure.DEFAULT_INPUT_METHOD);
            boolean ok=true;
            if((getPackageName()+"/.ProbeIme").equals(current))ok=Settings.Secure.putString(getContentResolver(),Settings.Secure.DEFAULT_INPUT_METHOD,previous);
            ProbeLog.event("ime_restored","ok",ok,"previous",previous);previous=null;
        } else handler.postDelayed(this,500);
    }};
    @Override public void onCreate(){super.onCreate();instance=this;}
    @Override public void onDestroy(){handler.removeCallbacksAndMessages(null);instance=null;super.onDestroy();}
    @Override public boolean onEvaluateInputViewShown(){return false;}
    @Override public void onStartInput(EditorInfo i,boolean restart){super.onStartInput(i,restart);ProbeLog.event("ime_start","pkg",i.packageName);}
    void command(String cmd,String text){
        InputConnection c=getCurrentInputConnection();
        if(c==null)throw new IllegalStateException("no input connection");
        boolean ok;
        if(cmd.equals("commit"))ok=c.commitText(text,1);
        else if(cmd.equals("compose"))ok=c.setComposingText(text,1);
        else ok=c.finishComposingText();
        ProbeLog.event("ime_command","cmd",cmd,"ok",ok);
    }
    void arm(String prev){
        if(prev==null || prev.equals(getPackageName()+"/.ProbeIme"))throw new IllegalArgumentException("invalid previous IME");
        previous=prev; heartbeat=SystemClock.elapsedRealtime();handler.removeCallbacks(watchdog);handler.postDelayed(watchdog,500);
        ProbeLog.event("restore_armed","previous",prev);
    }
    void beat(){heartbeat=SystemClock.elapsedRealtime();}
}
