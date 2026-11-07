package dev.lantern.probeshell;
import android.app.ActivityOptions;
import android.content.Intent;
import android.net.Uri;
import android.hardware.display.VirtualDisplay;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.media.MediaMuxer;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.Surface;
import java.util.*;

final class VdBench implements AutoCloseable {
    // Flag values from scrcpy v5.0.1 NewDisplayCapture (AOSP DisplayManager).
    static final Map<String,Integer> FLAGS=new LinkedHashMap<>();
    static {
        FLAGS.put("PUBLIC",1);FLAGS.put("PRESENTATION",2);FLAGS.put("OWN_CONTENT_ONLY",8);
        FLAGS.put("SUPPORTS_TOUCH",1<<6);FLAGS.put("ROTATES_WITH_CONTENT",1<<7);
        FLAGS.put("DESTROY_CONTENT_ON_REMOVAL",1<<8);FLAGS.put("TRUSTED",1<<10);
        FLAGS.put("OWN_DISPLAY_GROUP",1<<11);FLAGS.put("ALWAYS_UNLOCKED",1<<12);
        FLAGS.put("TOUCH_FEEDBACK_DISABLED",1<<13);FLAGS.put("OWN_FOCUS",1<<14);FLAGS.put("DEVICE_DISPLAY_GROUP",1<<15);
    }
    static final String DEFAULT_FLAGS="PUBLIC,PRESENTATION,OWN_CONTENT_ONLY,SUPPORTS_TOUCH,ROTATES_WITH_CONTENT,DESTROY_CONTENT_ON_REMOVAL,TRUSTED,OWN_DISPLAY_GROUP,TOUCH_FEEDBACK_DISABLED";
    VirtualDisplay display; MediaCodec codec; Surface surface;
    String mime; String encoder; int dpi=160; int fps=60;
    int width,height;
    final MediaCodec.BufferInfo info=new MediaCodec.BufferInfo();
    VdBench(int w,int h,int density,String codecName,String flags) throws Exception {
        dpi=density; mime=codecName.equals("h264") ? "video/avc":"video/hevc";
        encoder=codecName.equals("h264") ? "c2.qti.avc.encoder":"c2.qti.hevc.encoder";
        startCodec(w,h);
        int bits=0;
        for(String f:flags.split(",")){
            if(!FLAGS.containsKey(f))throw new IllegalArgumentException("unknown flag "+f);
            bits |= FLAGS.get(f);
        }
        try {
            Object dm=Hidden.service("DisplayManager");
            display=(VirtualDisplay)Hidden.call(dm,"createNewVirtualDisplay",
                new Class<?>[]{String.class,int.class,int.class,int.class,Surface.class,int.class},
                "LanternProbe",w,h,dpi,surface,bits);
            if(display==null)throw new IllegalStateException("null virtual display");
            if(id()<=0)throw new IllegalStateException("invalid display id");
            System.out.println("DISPLAY id="+id()+" flags="+bits+" names="+flags);
            imePolicy(2);
        }catch(Exception e){close();throw e;}
    }
    int id(){return display.getDisplay().getDisplayId();}
    void startCodec(int w,int h) throws Exception {
        width=w;height=h;
        codec=MediaCodec.createByCodecName(encoder);
        MediaFormat f=MediaFormat.createVideoFormat(mime,w,h);
        f.setInteger(MediaFormat.KEY_COLOR_FORMAT,MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
        f.setInteger(MediaFormat.KEY_BIT_RATE,8000000);f.setInteger(MediaFormat.KEY_FRAME_RATE,fps);
        f.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL,1);f.setInteger(MediaFormat.KEY_MAX_B_FRAMES,0);
        codec.configure(f,null,null,MediaCodec.CONFIGURE_FLAG_ENCODE);
        surface=codec.createInputSurface();codec.start();
    }
    void launch(String pkg) throws Exception {
        Intent i;
        if(pkg.equals("dev.lantern.probe")){
            i=new Intent().setClassName(pkg,pkg+".AnimActivity");
            i.setData(Uri.parse("lanternprobe://display/"+id()));
            i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK|Intent.FLAG_ACTIVITY_NEW_DOCUMENT|Intent.FLAG_ACTIVITY_MULTIPLE_TASK);
        }else {
            Object context=Hidden.call(Class.forName("com.genymobile.scrcpy.FakeContext"),"get",new Class<?>[]{});
            i=((android.content.Context)context).getPackageManager().getLaunchIntentForPackage(pkg);
            if(i==null)throw new IllegalArgumentException("not launchable "+pkg);
            i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        }
        ActivityOptions o=ActivityOptions.makeBasic().setLaunchDisplayId(id());
        Object result=Hidden.call(Hidden.service("ActivityManager"),"startActivity",new Class<?>[]{Intent.class,Bundle.class},i,o.toBundle());
        System.out.println("OK launch="+pkg+" display="+id()+" result="+result);
    }
    void imePolicy(int policy) throws Exception {
        Hidden.call(Hidden.service("WindowManager"),"setDisplayImePolicy",new Class<?>[]{int.class,int.class},id(),policy);
        System.out.println("OK ime_policy="+policy+" display="+id());
    }
    void stopCodec(){
        if(codec!=null){try{codec.stop();}catch(Exception ignored){}codec.release();codec=null;}
        if(surface!=null){surface.release();surface=null;}
    }
    long drain(long timeoutMs,String recording) throws Exception {
        MediaMuxer muxer=recording==null?null:new MediaMuxer(recording,MediaMuxer.OutputFormat.MUXER_OUTPUT_MPEG_4);
        int track=-1;long start=SystemClock.elapsedRealtime(),first=-1;int frames=0;
        try{
            while(!Thread.currentThread().isInterrupted() && SystemClock.elapsedRealtime()-start<timeoutMs){
                int index=codec.dequeueOutputBuffer(info,20000);
                if(index==MediaCodec.INFO_OUTPUT_FORMAT_CHANGED){
                    if(muxer!=null){track=muxer.addTrack(codec.getOutputFormat());muxer.start();}
                }else if(index>=0){
                    boolean isFrame=info.size>0 && (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG)==0;
                    if(isFrame){
                        if(first<0)first=SystemClock.elapsedRealtime();
                        frames++;
                        if(muxer!=null && track>=0)muxer.writeSampleData(track,codec.getOutputBuffer(index),info);
                    }
                    codec.releaseOutputBuffer(index,false);
                    if(isFrame && recording==null)break;
                }
            }
        }finally{if(muxer!=null){if(track>=0)muxer.stop();muxer.release();}}
        if(first<0)throw new IllegalStateException("no encoded frame in "+timeoutMs+"ms");
        if(recording!=null)System.out.println("OK recording="+recording+" frames="+frames+" duration_ms="+(SystemClock.elapsedRealtime()-start));
        return first;
    }
    void resize(int w,int h,int seq) throws Exception {
        long start=SystemClock.elapsedRealtime();
        display.setSurface(null);stopCodec();
        display.resize(w,h,dpi);startCodec(w,h);display.setSurface(surface);
        long first=drain(5000,null);
        System.out.println("event=resize seq="+seq+" start_ms="+start+" first_ms="+first+" ms_to_first_frame="+(first-start)+" w="+w+" h="+h);
    }
    public void close(){
        if(display!=null){try{display.release();}catch(Exception e){System.out.println("ERR release_display="+e);}display=null;}
        stopCodec();
    }
}
