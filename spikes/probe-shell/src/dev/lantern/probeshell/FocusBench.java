package dev.lantern.probeshell;
import android.os.SystemClock;
import android.view.InputEvent;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.InputDevice;
final class FocusBench {
    static void inject(InputEvent e,int display) throws Exception {
        Hidden.call(e,"setDisplayId",new Class<?>[]{int.class},display);
        Object input=Hidden.service("InputManager");
        boolean ok=(Boolean)Hidden.call(input,"injectInputEvent",new Class<?>[]{InputEvent.class,int.class},e,2);
        if(!ok)throw new IllegalStateException("injection rejected");
    }
    static void key(int display,int code) throws Exception {
        long t=SystemClock.uptimeMillis();
        inject(new KeyEvent(t,t,KeyEvent.ACTION_DOWN,code,0,0,-1,0,0,InputDevice.SOURCE_KEYBOARD),display);
        inject(new KeyEvent(t,t,KeyEvent.ACTION_UP,code,0,0,-1,0,0,InputDevice.SOURCE_KEYBOARD),display);
    }
    static void focus(String method,int task,int display) throws Exception {
        if(display<=0)throw new IllegalArgumentException("positive probe display required");
        long requested=SystemClock.elapsedRealtime();
        System.out.println("event=focus_request display="+display+" elapsed_ms="+requested+" method="+method);
        if(method.equals("tap")){
            long t=SystemClock.uptimeMillis();
            MotionEvent e=MotionEvent.obtain(t,t,MotionEvent.ACTION_DOWN,30,30,0);e.setSource(InputDevice.SOURCE_TOUCHSCREEN);
            try{inject(e,display);e.setAction(MotionEvent.ACTION_UP);inject(e,display);}finally{e.recycle();}
        }else {
            Object atm=Hidden.binder("android.app.ActivityTaskManager");
            if(method.equals("settask"))Hidden.call(atm,"setFocusedTask",new Class<?>[]{int.class},task);
            else if(method.equals("display"))Hidden.call(Hidden.call(Class.forName("android.view.WindowManagerGlobal"),"getWindowManagerService",new Class<?>[]{}),"setFocusedDisplay",new Class<?>[]{int.class},display);
            else throw new IllegalArgumentException("unknown focus method "+method);
        }
    }
}
