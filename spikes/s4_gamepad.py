"""Real-controller routing: UHID pad (via scrcpy window) must follow setFocusedTask."""
import json
import subprocess
import time
from common import ROOT, SERIAL, SCRCPY, adb, parse_event
from session import Session

PHASE_S = 20

def logs():
    return [e for l in adb('logcat', '-d', '-v', 'raw', '-s', 'LanternProbe:I', '*:S').splitlines()
            if (e := parse_event(l))]

def main():
    directory = ROOT / 'raw' / f's4pad_{time.time_ns()}'; directory.mkdir(parents=True)
    adb('logcat', '-c')
    flags = ('flags=PUBLIC,PRESENTATION,OWN_CONTENT_ONLY,SUPPORTS_TOUCH,ROTATES_WITH_CONTENT,'
             'DESTROY_CONTENT_ON_REMOVAL,TRUSTED,OWN_DISPLAY_GROUP,ALWAYS_UNLOCKED,'
             'TOUCH_FEEDBACK_DISABLED,OWN_FOCUS')
    result = {}
    with Session(flags) as a, Session(flags) as b:
        time.sleep(1)
        ev = logs()
        task = {d: max(e['task'] for e in ev if e.get('event') == 'resumed' and e.get('display') == d)
                for d in (a.display, b.display)}
        a.command(f'focus settask {task[a.display]} {a.display}')
        win = subprocess.Popen([SCRCPY, '-s', SERIAL, f'--display-id={a.display}', '--gamepad=uhid',
                                '--keyboard=disabled', '--mouse=disabled', '--no-audio',
                                '--window-title=LANTERN GAMEPAD TEST - keep focused, press A'],
                               stdout=open(directory / 'scrcpy.log', 'w'), stderr=subprocess.STDOUT)
        try:
            phases = []
            for name, target in (('A', a), ('B', b)):
                target.command(f'focus settask {task[target.display]} {target.display}')
                start = max((e.get('elapsed_ms', 0) for e in logs()), default=0)
                print(f'PHASE {name} focused display {target.display} for {PHASE_S}s', flush=True)
                time.sleep(PHASE_S)
                keys = [e for e in logs() if e.get('event') == 'key' and e.get('elapsed_ms', 0) > start
                        and e.get('action') == 0]
                pad = [e for e in keys if (e.get('source', 0) & 0x400) == 0x400]
                phases.append({'phase': name, 'target': target.display,
                               'pad_presses_by_display': {d: sum(1 for e in pad if e['display'] == d)
                                                          for d in (a.display, b.display)},
                               'codes': sorted({e['code'] for e in pad})})
                print(json.dumps(phases[-1]), flush=True)
            result = {'phases': phases,
                      'pass': all(p['pad_presses_by_display'][p['target']] > 0 and
                                  sum(p['pad_presses_by_display'].values()) == p['pad_presses_by_display'][p['target']]
                                  for p in phases)}
        finally:
            win.terminate()
            try: win.wait(timeout=5)
            except subprocess.TimeoutExpired: win.kill(); win.wait()
    (directory / 'input-devices.txt').write_text(adb('shell', 'dumpsys input'))
    (directory / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result)); print('RAW', directory)

if __name__ == '__main__':
    main()
