package dev.lantern.probeshell;
import android.os.Build;
import android.os.Looper;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.util.*;

public class Main {
    static Map<String,String> options(String[] a){
        Map<String,String> out=new HashMap<>();
        for(String s:a){int i=s.indexOf('=');if(i>0)out.put(s.substring(0,i),s.substring(i+1));}
        return out;
    }
    static VdBench vd(Map<String,String> o) throws Exception {
        String[] size=o.getOrDefault("size","1280x800").split("x");
        return new VdBench(Integer.parseInt(size[0]),Integer.parseInt(size[1]),Integer.parseInt(o.getOrDefault("dpi","160")),o.getOrDefault("codec","h265"),o.getOrDefault("flags",VdBench.DEFAULT_FLAGS));
    }
    static void run(String[] a) throws Exception {
        Map<String,String> o=options(a);
        if(a[0].equals("vd") && a.length>1 && a[1].equals("resize-bench")){
            try(VdBench v=vd(o)){
                v.launch("dev.lantern.probe");v.drain(5000,null);
                String[] sizes=o.getOrDefault("sizes","1280x800,1600x900,1000x1000,800x1280").split(",");
                int reps=Integer.parseInt(o.getOrDefault("reps","20")),seq=0;
                for(int i=0;i<reps;i++)for(String s:sizes){String[] wh=s.split("x");v.resize(Integer.parseInt(wh[0]),Integer.parseInt(wh[1]),++seq);}
                System.out.println("OK resized="+seq);
            }
        } else if(a[0].equals("vd") && a.length>1 && a[1].equals("create")){
            try(VdBench v=vd(o)){
                v.launch(o.getOrDefault("pkg","dev.lantern.probe"));
                if(o.containsKey("record"))v.drain(Long.parseLong(o.getOrDefault("duration_ms","8000")),o.get("record"));
                else {
                    // Drain on a worker while stdin commands drive focus/IME; EOF owns teardown.
                    Thread drain=new Thread(()->{try{while(!Thread.currentThread().isInterrupted())v.drain(1000,null);}catch(Exception e){System.out.println("ERR drain="+e);}});
                    drain.start();
                    try {
                        BufferedReader in=new BufferedReader(new InputStreamReader(System.in));String line;
                        System.out.println("OK ready="+v.id());
                        while((line=in.readLine())!=null){
                            String[] parts=line.trim().split(" +");
                            try{
                                if(parts[0].equals("quit"))break;
                                if(parts[0].equals("policy"))v.imePolicy(Integer.parseInt(parts[1]));
                                else if(parts[0].equals("key"))FocusBench.key(Integer.parseInt(parts[1]),Integer.parseInt(parts[2]));
                                else if(parts[0].equals("focus"))FocusBench.focus(parts[1],Integer.parseInt(parts[2]),Integer.parseInt(parts[3]));
                                else if(parts[0].equals("launch"))v.launch(parts[1]);
                                else throw new IllegalArgumentException("unknown input command");
                                System.out.println("OK command="+line);
                            }catch(Exception e){System.out.println("ERR command="+line+" error="+e);}
                        }
                    } finally {drain.interrupt();drain.join(3000);}
                }
            }
        }else throw new IllegalArgumentException("unknown command");
    }
    public static void main(String[] args) throws Exception {
        if(args.length==1 && args[0].equals("ping")){
            System.out.println("OK sdk="+Build.VERSION.SDK_INT+" model="+Build.MODEL);return;
        }
        if(args.length==0){System.err.println("ERR command required");System.exit(2);}
        // app_process has no app-owned main looper. Match scrcpy's quit-allowed setup.
        Looper.prepare();
        java.lang.reflect.Field main=Looper.class.getDeclaredField("sMainLooper");
        main.setAccessible(true);main.set(null,Looper.myLooper());
        Hidden.call(Class.forName("com.genymobile.scrcpy.Workarounds"),"apply",new Class<?>[]{});
        final int[] status={0};
        new Thread(()->{
            try{run(args);}catch(Throwable e){e.printStackTrace(System.out);status[0]=1;}
            finally{Looper.getMainLooper().quitSafely();}
        },"lantern-probe").start();
        Looper.loop();
        System.exit(status[0]);
    }
}
