"""Host ownership for an interactive probe. EOF is the release signal."""
import queue
import threading
import time
from common import shell_probe

class Session:
    def __init__(self,*args):
        self.process=shell_probe('vd','create',*args)
        self.lines=[]
        assert self.process.stdout is not None and self.process.stdin is not None
        self.pending=queue.Queue()
        def read():
            for line in self.process.stdout:
                self.lines.append(line.rstrip()); self.pending.put(line.rstrip())
            self.pending.put(None)
        self.reader=threading.Thread(target=read,daemon=True);self.reader.start()
        self.display=None
        try:
            line=self.until(lambda l:l.startswith('OK ready='),timeout=15)
            self.display=int(line.split('=')[1])
            if self.display<=0:raise RuntimeError('invalid display')
        except BaseException:
            self.close();raise
    def until(self,predicate,timeout=8):
        end=time.monotonic()+timeout
        while True:
            left=end-time.monotonic()
            if left<=0:raise TimeoutError('probe did not acknowledge; '+repr(self.lines[-8:]))
            try:line=self.pending.get(timeout=left)
            except queue.Empty:raise TimeoutError('probe stalled; '+repr(self.lines[-8:]))
            if line is None:raise RuntimeError('probe exited; '+'\n'.join(self.lines[-30:]))
            if predicate(line):return line
            if line.startswith('ERR command='):raise RuntimeError(line)
    def command(self,line):
        self.process.stdin.write(line+'\n');self.process.stdin.flush()
        return self.until(lambda l:l=='OK command='+line)
    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:self.process.wait(timeout=5)
            except Exception:
                self.process.terminate()
                raise RuntimeError('probe did not exit within 5s of EOF')
        self.reader.join(timeout=1)
    def __enter__(self):return self
    def __exit__(self,*exc):self.close()
