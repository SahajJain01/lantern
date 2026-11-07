package dev.lantern.probe;
import android.app.Activity;
import android.os.Bundle;
import android.content.Intent;
import android.content.res.Configuration;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.WindowManager;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.text.TextWatcher;
import android.text.Editable;

public class AnimActivity extends Activity {
    private EditText edit;
    private int display(){return getDisplay().getDisplayId();}
    private void logResume(){ProbeLog.event("resumed", "display", display(), "task", getTaskId(), "from", getIntent().getStringExtra("from"));}
    @Override public void onCreate(Bundle state){
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        LinearLayout column=new LinearLayout(this); column.setOrientation(LinearLayout.VERTICAL);
        edit=new EditText(this); edit.setSingleLine(true); edit.setHint("Lantern Unicode / focus probe");
        edit.addTextChangedListener(new TextWatcher(){
            public void beforeTextChanged(CharSequence s,int a,int c,int d){}
            public void onTextChanged(CharSequence s,int a,int b,int c){ProbeLog.event("text", "display",display(),"value",s.toString());}
            public void afterTextChanged(Editable e){}
        });
        column.addView(edit,new LinearLayout.LayoutParams(-1,-2));
        column.addView(new Counter(),new LinearLayout.LayoutParams(-1,0,1));
        setContentView(column);
        edit.requestFocus();
        edit.postDelayed(()-> ((InputMethodManager)getSystemService(INPUT_METHOD_SERVICE)).showSoftInput(edit,InputMethodManager.SHOW_IMPLICIT),500);
    }
    @Override protected void onResume(){super.onResume();logResume();}
    @Override protected void onNewIntent(Intent i){super.onNewIntent(i);setIntent(i);logResume();}
    @Override public void onConfigurationChanged(Configuration c){super.onConfigurationChanged(c);ProbeLog.event("config", "display",display(),"width_dp",c.screenWidthDp,"height_dp",c.screenHeightDp);}
    @Override public boolean dispatchKeyEvent(KeyEvent e){ProbeLog.event("key","display",display(),"code",e.getKeyCode(),"action",e.getAction(),"device",e.getDeviceId(),"source",e.getSource());return super.dispatchKeyEvent(e);}
    @Override public boolean dispatchGenericMotionEvent(MotionEvent e){ProbeLog.event("motion","display",display(),"source",e.getSource());return super.dispatchGenericMotionEvent(e);}
    class Counter extends View {
        final Paint p=new Paint(Paint.ANTI_ALIAS_FLAG); int count; int lastW,lastH;
        Counter(){super(AnimActivity.this);}
        @Override protected void onDraw(Canvas c){
            super.onDraw(c); count++;
            c.drawColor(Color.rgb(15,30,50)); p.setColor(Color.WHITE);p.setTextSize(55);
            c.drawText("Lantern  display="+display()+" frame="+count,25,100,p);
            c.drawText("elapsed="+android.os.SystemClock.elapsedRealtime(),25,180,p);
            p.setColor(Color.rgb(250,180,40)); int x=(count*9)%Math.max(1,getWidth()); c.drawRect(x,200,x+55,getHeight(),p);
            if(getWidth()!=lastW || getHeight()!=lastH){lastW=getWidth();lastH=getHeight();ProbeLog.event("draw_size","display",display(),"w",lastW,"h",lastH,"frame",count);}
            postInvalidateOnAnimation();
        }
    }
}
