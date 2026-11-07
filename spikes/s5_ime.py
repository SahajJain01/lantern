"""Probe Unicode/compose under each display IME policy, with journaled restoration."""
import base64
import json
import threading
import time
from common import ROOT, shell, adb, settings, save_private, parse_event
from session import Session

PACKAGE='dev.lantern.probe'
COMP=PACKAGE+'/.ProbeIme'
TEXT='Xin chào, Tiếng Việt ạ 👋'

def cmd(name, **extras):
    args=['am','broadcast','--user','0','-n',PACKAGE+'/.CmdReceiver','--es','cmd',name]
    for k,v in extras.items(): args += ['--ei' if isinstance(v,int) else '--es',k,str(v)]
    out=shell(*args)
    if 'result=1' in out or 'Permission Denial' in out: raise RuntimeError(out)
    return out

def events():
    return [e for l in adb('logcat','-d','-v','raw','-s','LanternProbe:I','*:S').splitlines() if (e:=parse_event(l))]

def restore(before):
    # Restore only session-owned keys; never overwrite whole settings namespaces.
    for k in ('default_input_method','enabled_input_methods','selected_input_method_subtype'):
        if k in before: shell('settings','--user','0','put','secure',k,before[k])
        else: shell('settings','--user','0','delete','secure',k)
    shell('ime','set',before['default_input_method'])

def main():
    directory=ROOT/'raw'/f's5_{time.time_ns()}';directory.mkdir(parents=True)
    before=settings('secure');save_private(directory/'restore.json',before)
    results=[]
    try:
        shell('pm','grant','--user','0',PACKAGE,'android.permission.WRITE_SECURE_SETTINGS')
        print(shell('ime','enable',COMP),flush=True);print(shell('ime','set',COMP),flush=True)
        with Session() as s:
            for policy in (0,1,2):
                s.command('policy '+str(policy));s.command('focus tap 0 '+str(s.display));time.sleep(.6)
                start=max((e.get('elapsed_ms',0) for e in events()),default=0)
                print(cmd('commit',text64=base64.b64encode(TEXT.encode()).decode()),flush=True)
                print(cmd('compose',text64=base64.b64encode('việ'.encode()).decode()),flush=True)
                print(cmd('finish_compose'),flush=True)
                after=[e for e in events() if e.get('elapsed_ms',0)>start]
                result={'policy':policy,'display':s.display,'texts':[e.get('value') for e in after if e.get('event')=='text'],
                        'errors':[e for e in after if e.get('event')=='command_error']}
                result['unicode_seen']=any(TEXT in v for v in result['texts'])
                result['compose_seen']=any(TEXT+'việ' in v for v in result['texts'])
                results.append(result);print(json.dumps(result,ensure_ascii=False),flush=True)
                # dumps are private and preserve actual IME visibility/target state for inspection.
                (directory/f'window-policy-{policy}.txt').write_text(shell('dumpsys','window'))
            print(cmd('arm_restore',prev=before['default_input_method']),flush=True)
            # No heartbeat is sent: exercise app-side recovery independent of an adb command.
            time.sleep(8)
            current=settings('secure').get('default_input_method')
            results.append({'recovery_test':'heartbeat-timeout','restored':current==before['default_input_method'],'actual':current,
                            'physical_unplug_test':'not performed'})
    finally:
        restore(before)
        shell('pm','revoke','--user','0',PACKAGE,'android.permission.WRITE_SECURE_SETTINGS')
        (directory/'events.log').write_text(adb('logcat','-d','-v','raw','-s','LanternProbe:I','*:S'))
        (directory/'result.json').write_text(json.dumps(results,indent=2,ensure_ascii=False))
    print('RAW',directory)
if __name__=='__main__':main()
