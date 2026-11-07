"""Test display focus with unattributed keyboard events. Never touch display 0."""
import argparse
import json
import time
from common import ROOT,adb,parse_event
from session import Session

BASE='PUBLIC,PRESENTATION,OWN_CONTENT_ONLY,SUPPORTS_TOUCH,ROTATES_WITH_CONTENT,DESTROY_CONTENT_ON_REMOVAL,TRUSTED,OWN_DISPLAY_GROUP,TOUCH_FEEDBACK_DISABLED'

def logs():
    return [e for l in adb('logcat','-d','-v','raw','-s','LanternProbe:I','*:S').splitlines() if (e:=parse_event(l))]

def main():
    p=argparse.ArgumentParser();p.add_argument('--reps',type=int,default=20);a=p.parse_args()
    directory=ROOT/'raw'/f's4_{time.time_ns()}';directory.mkdir(parents=True)
    results=[]
    for ownfocus in (False,True):
        flag=BASE+(',OWN_FOCUS,DEVICE_DISPLAY_GROUP' if ownfocus else '')
        with Session('flags='+flag) as one, Session('flags='+flag) as two:
            time.sleep(.5)
            events=logs()
            tasks={d:max(e['task'] for e in events if e.get('event')=='resumed' and e.get('display')==d) for d in (one.display,two.display)}
            for method in ('settask','display','tap'):
                for i in range(a.reps):
                    target=(one,two)[i%2]
                    before=logs()
                    last=max((e.get('elapsed_ms',0) for e in before),default=0)
                    start=time.monotonic()
                    try:
                        target.command(f'focus {method} {tasks[target.display]} {target.display}')
                        target.command('key -1 29')
                    except Exception as e:
                        results.append({'method':method,'own_focus':ownfocus,'error':str(e)});break
                    # Injection is synchronous WAIT_FOR_FINISH, so log readback includes recipients.
                    after=logs()
                    keys=[e for e in after if e.get('event')=='key' and e.get('code')==29 and e.get('action')==0 and e.get('elapsed_ms',0)>last]
                    row={'method':method,'own_focus':ownfocus,'rep':i,'target':target.display,
                         'got':[e['display'] for e in keys], 'host_roundtrip_ms':round((time.monotonic()-start)*1000,2)}
                    results.append(row);print(json.dumps(row),flush=True)
            (directory/f'shell_{ownfocus}.log').write_text('\n'.join(one.lines+two.lines))
    (directory/'result.json').write_text(json.dumps(results,indent=2));print('RAW',directory)
if __name__=='__main__':main()
