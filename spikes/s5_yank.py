"""Physical-unplug IME recovery. Heartbeats stop when USB is pulled; the phone must restore alone."""
import json
import subprocess
import time
from common import ROOT, ADB, SERIAL, adb, shell, settings, save_private

PKG = 'dev.lantern.probe'
COMP = PKG + '/.ProbeIme'

def bound_ime():
    out = shell('dumpsys', 'input_method')
    return [l.strip() for l in out.splitlines() if 'mCurMethodId=' in l or 'mCurId=' in l][:2]

def main():
    directory = ROOT / 'raw' / f's5yank_{time.time_ns()}'; directory.mkdir(parents=True)
    before = settings('secure'); save_private(directory / 'restore.json', before)
    prev = before['default_input_method']
    result = {'previous': prev}
    try:
        shell('pm', 'grant', '--user', '0', PKG, 'android.permission.WRITE_SECURE_SETTINGS')
        shell('ime', 'enable', COMP); shell('ime', 'set', COMP)
        time.sleep(1)
        out = shell('am', 'broadcast', '--user', '0', '-n', PKG + '/.CmdReceiver', '--es', 'cmd', 'arm_restore', '--es', 'prev', prev)
        if 'result=0' not in out:
            raise RuntimeError('arm failed: ' + out)
        result['armed_ime'] = settings('secure').get('default_input_method')
        result['armed_bound'] = bound_ime()
        print('ARMED', json.dumps(result), flush=True)
        print('UNPLUG NOW', flush=True)
        beats = 0
        while True:
            p = subprocess.run([ADB, '-s', SERIAL, 'shell', 'am', 'broadcast', '--user', '0', '-n', PKG + '/.CmdReceiver',
                                '--es', 'cmd', 'heartbeat'], capture_output=True, text=True, timeout=10)
            if p.returncode != 0 or 'result=0' not in p.stdout:
                break
            beats += 1
            time.sleep(1)
        lost = time.time(); result['heartbeats_sent'] = beats
        print('USB LOST — waiting for replug', flush=True)
        subprocess.run([ADB, '-s', SERIAL, 'wait-for-device'], timeout=600)
        result['replugged_after_s'] = round(time.time() - lost, 1)
        time.sleep(2)
        result['after_setting'] = settings('secure').get('default_input_method')
        result['after_bound'] = bound_ime()
        result['events'] = [l for l in adb('logcat', '-d', '-v', 'raw', '-s', 'LanternProbe:I', '*:S').splitlines()
                            if 'ime_restored' in l or 'restore_armed' in l]
        result['pass'] = result['after_setting'] == prev and any(prev in l for l in result['after_bound'])
    finally:
        try:
            for k in ('default_input_method', 'enabled_input_methods', 'selected_input_method_subtype', 'input_methods_subtype_history'):
                if k in before: shell('settings', '--user', '0', 'put', 'secure', k, before[k])
                else: shell('settings', '--user', '0', 'delete', 'secure', k)
            shell('ime', 'set', prev)
            shell('pm', 'revoke', '--user', '0', PKG, 'android.permission.WRITE_SECURE_SETTINGS')
        except Exception as e:
            result['cleanup_error'] = str(e)
        (directory / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2)); print('RAW', directory)

if __name__ == '__main__':
    main()
