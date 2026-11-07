"""Run the real resize bench and summarize actual emitted events."""
import argparse
import json
import math
import statistics
import time
from common import ROOT,shell_probe,parse_event,adb

def main():
    p=argparse.ArgumentParser();p.add_argument('--reps',type=int,default=20);a=p.parse_args()
    proc=shell_probe('vd','resize-bench',f'reps={a.reps}')
    try:out,_=proc.communicate(timeout=180)
    except BaseException:
        proc.kill();proc.wait();raise
    directory=ROOT/'raw'/f's6_{time.time_ns()}';directory.mkdir(parents=True)
    (directory/'shell.log').write_text(out)
    app=adb('logcat','-d','-v','raw','-s','LanternProbe:I','*:S')
    (directory/'app.log').write_text(app)
    events=[e for line in out.splitlines() if (e:=parse_event(line)) and e.get('event')=='resize']
    result={'exit':proc.returncode,'count':len(events),'expected':4*a.reps,'sizes':{},'events':events}
    for wh in sorted({(e['w'],e['h']) for e in events}):
        vals=sorted(e['ms_to_first_frame'] for e in events if (e['w'],e['h'])==wh)
        result['sizes'][f'{wh[0]}x{wh[1]}']={'min':min(vals),'median':statistics.median(vals),'p95':vals[math.ceil(.95*len(vals))-1]}
    result['pass']=proc.returncode==0 and len(events)==4*a.reps and statistics.median([e['ms_to_first_frame'] for e in events])<=150
    (directory/'result.json').write_text(json.dumps(result,indent=2))
    print(json.dumps({k:v for k,v in result.items() if k!='events'},indent=2));print('RAW',directory)
    if proc.returncode:print(out[-10000:])
    return int(not result['pass'])
if __name__=='__main__':raise SystemExit(main())
