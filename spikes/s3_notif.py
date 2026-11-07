"""Local synthetic notification delivery; does not send a message to anyone."""
import json
import time
from common import ROOT,shell,settings,parse_event,adb,save_private
from session import Session
from s5_ime import cmd, events

PKG='dev.lantern.probe'
NLS=PKG+'/.ProbeNls'

def main():
    directory=ROOT/'raw'/f's3_{time.time_ns()}';directory.mkdir(parents=True)
    prior=settings('secure');save_private(directory/'restore.json',prior)
    result={}
    try:
        shell('pm','grant','--user','0',PKG,'android.permission.POST_NOTIFICATIONS')
        print(shell('cmd','notification','allow_listener',NLS,'0'),flush=True)
        time.sleep(1)
        with Session() as s:
            old=max((e.get('elapsed_ms',0) for e in events()),default=0)
            print(cmd('notify'),flush=True);time.sleep(1)
            found=[e for e in events() if e.get('event')=='notification' and e.get('pkg')==PKG and e.get('elapsed_ms',0)>old]
            if not found:raise RuntimeError('probe notification not observed')
            key=found[-1]['key']
            print(cmd('fire',key=key,display=s.display),flush=True);time.sleep(1)
            after=[e for e in events() if e.get('elapsed_ms',0)>old]
            targeted=[e for e in after if e.get('event')=='resumed' and e.get('from')=='notif']
            result={'target_display':s.display,'resumes':targeted,'pass':any(e.get('display')==s.display for e in targeted),
                    'command_errors':[e for e in after if e.get('event')=='command_error'],
                    'real_app_notification':'unverified (no third-party messaging app installed)'}
            (directory/'activities.txt').write_text(shell('dumpsys','activity','activities'))
            (directory/'shell.log').write_text('\n'.join(s.lines))
    finally:
        print(shell('cmd','notification','disallow_listener',NLS,'0'),flush=True)
        # Preserve user-populated list exactly; this component did not exist before the probe install.
        k='enabled_notification_listeners'
        if k in prior:shell('settings','--user','0','put','secure',k,prior[k])
        else:shell('settings','--user','0','delete','secure',k)
        shell('pm','revoke','--user','0',PKG,'android.permission.POST_NOTIFICATIONS')
        (directory/'events.log').write_text(adb('logcat','-d','-v','raw','-s','LanternProbe:I','*:S'))
        (directory/'result.json').write_text(json.dumps(result,indent=2))
    print(json.dumps(result,indent=2));print('RAW',directory)
if __name__=='__main__':main()
