package dev.lantern.probe;
import android.app.ActivityOptions;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;
import java.util.concurrent.ConcurrentHashMap;
public class ProbeNls extends NotificationListenerService {
    static ProbeNls instance;
    static final ConcurrentHashMap<String,StatusBarNotification> notifications=new ConcurrentHashMap<>();
    @Override public void onListenerConnected(){instance=this;for(StatusBarNotification n:getActiveNotifications())notifications.put(n.getKey(),n);ProbeLog.event("nls_connected");}
    @Override public void onListenerDisconnected(){instance=null;notifications.clear();}
    @Override public void onNotificationPosted(StatusBarNotification n){notifications.put(n.getKey(),n);ProbeLog.event("notification","key",n.getKey(),"pkg",n.getPackageName());}
    @Override public void onNotificationRemoved(StatusBarNotification n){notifications.remove(n.getKey());}
    void fire(String key,int display) throws Exception {
        if(display<=0)throw new IllegalArgumentException("positive probe display required");
        StatusBarNotification n=notifications.get(key);
        if(n==null || n.getNotification().contentIntent==null)throw new IllegalArgumentException("no contentIntent for key");
        ActivityOptions o=ActivityOptions.makeBasic().setLaunchDisplayId(display);
        o.setPendingIntentBackgroundActivityStartMode(ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED);
        n.getNotification().contentIntent.send(this,0,null,null,null,null,o.toBundle());
        ProbeLog.event("notification_fired","key",key,"display",display);
    }
}
