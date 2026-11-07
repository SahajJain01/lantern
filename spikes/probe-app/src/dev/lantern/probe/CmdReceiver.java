package dev.lantern.probe;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Base64;
import java.nio.charset.StandardCharsets;
public class CmdReceiver extends BroadcastReceiver {
    @Override public void onReceive(Context c,Intent i){
        String cmd=i.getStringExtra("cmd");
        try {
            if(cmd==null)throw new IllegalArgumentException("missing cmd");
            switch(cmd){
                case "notify":{
                    NotificationManager nm=c.getSystemService(NotificationManager.class);
                    nm.createNotificationChannel(new NotificationChannel("probe","Lantern probe",NotificationManager.IMPORTANCE_DEFAULT));
                    Intent target=new Intent(c,AnimActivity.class).putExtra("from","notif").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK|Intent.FLAG_ACTIVITY_MULTIPLE_TASK);
                    PendingIntent pi=PendingIntent.getActivity(c,1,target,PendingIntent.FLAG_IMMUTABLE|PendingIntent.FLAG_UPDATE_CURRENT);
                    nm.notify(1,new Notification.Builder(c,"probe").setSmallIcon(android.R.drawable.ic_dialog_info).setContentTitle("Lantern test").setContentText("Probe notification — local only").setContentIntent(pi).build());
                    break;
                }
                case "keys":for(String key:ProbeNls.notifications.keySet())ProbeLog.event("notification_key","key",key,"pkg",ProbeNls.notifications.get(key).getPackageName());break;
                case "fire":if(ProbeNls.instance==null)throw new IllegalStateException("listener not connected");ProbeNls.instance.fire(i.getStringExtra("key"),i.getIntExtra("display",-1));break;
                case "commit":case "compose":case "finish_compose":{
                    if(ProbeIme.instance==null)throw new IllegalStateException("IME not running");
                    String text=i.hasExtra("text64") ? new String(Base64.decode(i.getStringExtra("text64"),Base64.DEFAULT),StandardCharsets.UTF_8) : i.getStringExtra("text");
                    ProbeIme.instance.command(cmd,text==null?"":text);break;
                }
                case "arm_restore":if(ProbeIme.instance==null)throw new IllegalStateException("IME not running");ProbeIme.instance.arm(i.getStringExtra("prev"));break;
                case "heartbeat":if(ProbeIme.instance!=null)ProbeIme.instance.beat();break;
                default:throw new IllegalArgumentException("unknown command "+cmd);
            }
            ProbeLog.event("command_ok","cmd",cmd);setResultCode(0);
        }catch(Exception e){ProbeLog.event("command_error","cmd",cmd,"error",e.toString());setResultCode(1);setResultData(e.toString());}
    }
}
