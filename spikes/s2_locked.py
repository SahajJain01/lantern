"""Record probe displays while the real keyguard is showing. Does not set a PIN."""
import json
import subprocess
import time
from common import ROOT,adb,shell,shell_probe

FLAGS='PUBLIC,PRESENTATION,OWN_CONTENT_ONLY,SUPPORTS_TOUCH,ROTATES_WITH_CONTENT,DESTROY_CONTENT_ON_REMOVAL,TRUSTED,OWN_DISPLAY_GROUP,TOUCH_FEEDBACK_DISABLED'

def main():
    directory=ROOT/'raw'/f's2_{time.time_ns()}';directory.mkdir(parents=True)
    results=[]
    try:
        shell('input','keyevent','223');time.sleep(3)
        policy=shell('dumpsys','window','policy');(directory/'keyguard-before.txt').write_text(policy)
        for name,flags in [('normal',FLAGS),('always_unlocked',FLAGS+',ALWAYS_UNLOCKED')]:
            remote='/data/local/tmp/lantern-probe-s2-'+name+'.mp4'
            p=shell_probe('vd','create','flags='+flags,'record='+remote,'duration_ms=8000')
            try:out,_=p.communicate(timeout=25)
            except subprocess.TimeoutExpired:p.kill();p.wait();raise
            (directory/(name+'.log')).write_text(out)
            row={'flags':name,'exit':p.returncode}
            if p.returncode==0:
                movie=directory/(name+'.mp4');adb('pull',remote,movie);shell('rm','-f',remote)
                for sec in (2,6):
                    r=subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-ss',str(sec),'-i',str(movie),'-vf','extractplanes=y','-frames:v','1','-y',str(directory/(name+f'-{sec}.png'))],capture_output=True,text=True,timeout=20)
                    if r.returncode:row['extract_error']=r.stderr
            results.append(row)
        (directory/'keyguard-after.txt').write_text(shell('dumpsys','window','policy'))
    finally:
        shell('input','keyevent','224')
        (directory/'result.json').write_text(json.dumps(results,indent=2))
    print(json.dumps(results,indent=2));print('RAW',directory)
if __name__=='__main__':main()
