"""Bounded concurrent hardware-encoder measurement, no result synthesis."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import time
from common import ROOT, SERIAL, SCRCPY, adb, shell, require_device, settings, save_private

def measure(n, size, codec, fps, seconds, directory):
    run_dir = directory / f'{codec}_{size}_n{n}'
    run_dir.mkdir(parents=True, exist_ok=False)
    children = []
    encoder = 'c2.qti.hevc.encoder' if codec == 'h265' else 'c2.qti.avc.encoder'
    try:
        for i in range(n):
            movie = run_dir / f'{i}.mkv'
            log = open(run_dir / f'{i}.log', 'w')
            args = [SCRCPY, '-s', SERIAL, f'--new-display={size}/160',
                    '--start-app=dev.lantern.probe', '--no-window', '--no-audio',
                    '--no-vd-system-decorations', '--display-ime-policy=hide',
                    '--no-downsize-on-error', f'--port={28100+i}', f'--video-codec={codec}',
                    f'--video-encoder={encoder}', f'--max-fps={fps}',
                    f'--time-limit={seconds}', f'--record={movie}']
            if i == 0:
                args.append('--turn-screen-off')
            p = subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT)
            children.append((p, log, movie))
        deadline = time.monotonic() + seconds + 25
        for p, _, _ in children:
            p.wait(timeout=max(1, deadline - time.monotonic()))
    finally:
        for p, log, _ in children:
            if p.poll() is None:
                p.terminate()
                try: p.wait(timeout=7)
                except subprocess.TimeoutExpired:
                    p.kill(); p.wait()
            log.close()
    rows = []
    for i, (p, _, movie) in enumerate(children):
        text = (run_dir/f'{i}.log').read_text()
        ids = re.findall(r'New display: .*?\(id=(\d+)\)', text)
        row = {'n': n, 'size': size, 'codec': codec, 'encoder': encoder, 'i': i,
               'exit': p.returncode, 'display': int(ids[-1]) if ids else None,
               'errors': [l for l in text.splitlines() if 'ERROR' in l],
               'movie': str(movie)}
        if movie.exists():
            r = subprocess.run(['ffprobe','-v','error','-select_streams','v:0',
                '-count_frames','-show_entries','stream=nb_read_frames,width,height,duration:format=duration',
                '-of','json',str(movie)], capture_output=True, text=True, timeout=30)
            if r.returncode == 0:
                info=json.loads(r.stdout)
                if info.get('streams'):
                    st=info['streams'][0]
                    dur=float(st.get('duration',info.get('format',{}).get('duration',0)))
                    frames=int(st.get('nb_read_frames',0))
                    row.update(frames=frames,duration=dur,width=st.get('width'),height=st.get('height'),
                               fps=frames/dur if dur>0 else 0)
            else: row['ffprobe_error']=r.stderr
        rows.append(row)
    (run_dir/'result.json').write_text(json.dumps(rows,indent=2))
    for row in rows: print(json.dumps(row),flush=True)
    return rows

def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('--n',type=int,default=1)
    ap.add_argument('--size',default='1280x800')
    ap.add_argument('--codec',choices=['h264','h265'],default='h265')
    ap.add_argument('--fps',type=int,default=60)
    ap.add_argument('--seconds',type=int,default=30)
    ap.add_argument('--matrix',action='store_true')
    a=ap.parse_args()
    if not 1<=a.n<=6 or not re.fullmatch(r'[1-9]\d{2,3}x[1-9]\d{2,3}',a.size):
        ap.error('n must be 1..6; size must be e.g. 1280x800')
    require_device()
    directory=ROOT/'raw'/f's1_{time.time_ns()}'
    directory.mkdir(parents=True,mode=0o700)
    prior=settings('global')
    key='stay_on_while_plugged_in'
    save_private(directory/'restore.json',{'namespace':'global','key':key,'previous':prior.get(key)})
    rows=[]
    try:
        # Powering the physical panel off is not Android's SLEEP key (which would lock it).
        shell('settings','put','global',key,'7')
        combos=[(n,'1280x800',c) for c in ('h264','h265') for n in range(1,7)]
        combos += [(3,'1920x1080',c) for c in ('h264','h265')]
        for n,size,codec in combos if a.matrix else [(a.n,a.size,a.codec)]:
            rows.extend(measure(n,size,codec,a.fps,a.seconds,directory))
            time.sleep(2)
    finally:
        if key in prior: shell('settings','put','global',key,prior[key])
        else: shell('settings','delete','global',key)
        shell('input','keyevent','224')
    (directory/'all-results.json').write_text(json.dumps(rows,indent=2))
    print(f'RESULTS {directory}',flush=True)
    if any(r['exit'] or not r.get('frames') for r in rows):
        print('One or more streams failed; recorded as a result, not ignored.',flush=True)

if __name__=='__main__': main()
