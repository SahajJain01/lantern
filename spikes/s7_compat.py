"""Bounded visual checks of launchable apps, no automated touches or text."""
import csv
import json
import re
import subprocess
import time
from PIL import Image, ImageDraw
from common import ROOT,SERIAL,SCRCPY,shell,settings,save_private

def main():
    directory=ROOT/'raw'/f's7_{time.time_ns()}';directory.mkdir(parents=True)
    third={l.removeprefix('package:') for l in shell('pm','list','packages','--user','0','-3').splitlines()}
    entries=shell('cmd','package','query-activities','--user','0','--brief','-a','android.intent.action.MAIN','-c','android.intent.category.LAUNCHER')
    packages={m.group(1) for m in re.finditer(r'^\s*([\w.]+)/[^\s]+$',entries,re.M)}
    wanted=sorted((packages & third)-{'dev.phonedesk','dev.lantern.probe'})
    for pkg in ('org.lineageos.jelly','com.android.settings','com.android.messaging'):
        if pkg in packages:wanted.append(pkg)
    wanted=list(dict.fromkeys(wanted))
    before=settings('global');save_private(directory/'restore.json',before)
    rows=[];thumbs=[]
    try:
        shell('settings','put','global','stay_on_while_plugged_in','7')
        for pkg in wanted:
            movie=directory/(pkg+'.mkv');log=directory/(pkg+'.log');png=directory/(pkg+'.png')
            with log.open('w') as f:
                args=[SCRCPY,'-s',SERIAL,'--new-display=1280x800/160','--start-app='+pkg,
                    '--no-window','--no-audio','--time-limit=12','--record='+str(movie),
                    '--no-vd-system-decorations','--display-ime-policy=hide','--turn-screen-off']
                p=subprocess.run(args,stdout=f,stderr=subprocess.STDOUT,timeout=40)
            row={'package':pkg,'exit':p.returncode,'visual_status':'not inspected'}
            if movie.exists():
                r=subprocess.run(['ffmpeg','-hide_banner','-loglevel','error','-sseof','-1','-i',str(movie),
                    '-vf','extractplanes=y','-frames:v','1','-y',str(png)],capture_output=True,text=True,timeout=20)
                if r.returncode==0 and png.exists():
                    with Image.open(png) as image:
                        lo,hi=image.convert('L').getextrema()
                        row.update(luma_min=lo,luma_max=hi,
                                   visual_status='dark candidate' if hi-lo<10 else 'content candidate')
                        image.thumbnail((500,320));thumb=image.convert('RGB')
                        panel=Image.new('RGB',(520,360),'#ddd');panel.paste(thumb,(10,30))
                        ImageDraw.Draw(panel).text((10,5),pkg,fill='black');thumbs.append(panel)
                else:row['extract_error']=r.stderr
            rows.append(row);print(json.dumps(row),flush=True)
    finally:
        k='stay_on_while_plugged_in'
        if k in before:shell('settings','put','global',k,before[k])
        else:shell('settings','delete','global',k)
        shell('input','keyevent','224')
        (directory/'result.json').write_text(json.dumps(rows,indent=2))
    if thumbs:
        sheet=Image.new('RGB',(1040,360*((len(thumbs)+1)//2)),'white')
        for i,image in enumerate(thumbs):sheet.paste(image,((i%2)*520,(i//2)*360))
        sheet.save(directory/'contact-sheet.png')
    print('RAW',directory)
if __name__=='__main__':main()
