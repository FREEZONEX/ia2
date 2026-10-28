"""Real bridge subprocesses and a loopback HTTP IA2 stub; evidence stays in repair copy."""
import concurrent.futures
import hashlib
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import json
from pathlib import Path
import selectors
import subprocess
import sys
import threading
import time
import unittest
sys.dont_write_bytecode=True
sys.path.insert(0,str(Path(__file__).resolve().parent))
from p0_support import SRC,KEYS,envelope,payload,reg_for
from bridge.journal import Journal

OUT=SRC/'evidence'/'process'
if OUT.exists():
    n=2
    while (OUT.parent/('process-%d'%n)).exists(): n+=1
    OUT=OUT.parent/('process-%d'%n)

class Handler(BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def reply(self,body):
        raw=json.dumps(body).encode();self.send_response(200)
        self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
    def do_GET(self):
        st=self.server.state;st['reads'].append(self.path);n=time.monotonic_ns()//1000000
        if self.path=='/status':
            body=dict(runtime_id='fixture-R',scan_count=n,mode={'kind':'running'},forces=[],
                      watchdog_tripped=False,latch_reason=None,fault=None,device_health=[dict(name='dev',healthy=True)],
                      last_snapshot=dict(timestamp_us=n*1000,vars=[dict(name='sp',type_name='DINT',bits=0)]))
            if st['mode']=='missing-pre': body.pop('forces')
            if st['mode']=='bad-post' and st['posts']: body=[]
        elif self.path=='/health':
            body=dict(status='ok',fault=None,latch_reason=None,watchdog_tripped=False,
                      fieldbus_healthy=True,snapshot_tap={'running':True})
            if st['mode']=='thin-post' and st['posts']: body={}
        else: body=[]
        self.reply(body)
    def do_POST(self):
        body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.state['posts'].append(dict(body=body,origin=self.headers.get('X-IA2-Origin')))
        self.reply(dict(ok=True,name=body['name'],value=body['value'],undelivered_device=None))

def wait(fn,timeout=6):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        if fn():return True
        time.sleep(.02)
    return bool(fn())

def rows(path):
    if not path.exists():return []
    result=[]
    for line in path.read_text().splitlines():
        try:result.append(json.loads(line))
        except ValueError:pass
    return result

class Process(unittest.TestCase):
    def setUp(self):
        self.dest=OUT/self._testMethodName;self.dest.mkdir(parents=True,exist_ok=False)
        self.server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
        self.server.state=dict(posts=[],reads=[],mode='good')
        self.thread=threading.Thread(target=self.server.serve_forever,daemon=True);self.thread.start()
        self.procs=[]
        text=json.dumps(dict(version=1,targets=list(reg_for().targets.values())))
        (self.dest/'registry.json').write_text(text)
        (self.dest/'keys.json').write_text(json.dumps({'k':dict(secret_hex=KEYS['k']['secret'].hex(),not_before_ms=0,not_after_ms=2**60)}))
        (self.dest/'keys.json').chmod(0o600)
        (self.dest/'project.toml').write_text('[governance]\nwrite_mode = "allowlist"\n[[governance.rules]]\nvariable = "sp"\nmin = 0\nmax = 100\n')
        (self.dest/'requests.jsonl').write_text('')
        self.cmd=[sys.executable,'-I',str(SRC/'bridge_run.py'),'--registry',str(self.dest/'registry.json'),
                  '--registry-sha256',hashlib.sha256(text.encode()).hexdigest(),'--keys',str(self.dest/'keys.json'),
                  '--runtime','127.0.0.1:%d'%self.server.server_port,'--journal',str(self.dest/'journal.jsonl'),
                  '--trace',str(self.dest/'trace.jsonl'),'--requests',str(self.dest/'requests.jsonl'),'--project-dir',str(self.dest)]
    def tearDown(self):
        for p in self.procs:self.stop(p)
        (self.dest/'observed.json').write_text(json.dumps(self.server.state,indent=2)+'\n')
        self.server.shutdown();self.server.server_close();self.thread.join(timeout=3)
    def spawn(self,name,extra=()):
        with open(self.dest/(name+'.stderr'),'wb') as err:
            p=subprocess.Popen(self.cmd+list(extra),stdout=subprocess.PIPE,stderr=err)
        self.procs.append(p)
        sel=selectors.DefaultSelector();sel.register(p.stdout,selectors.EVENT_READ)
        ready=sel.select(6);sel.close()
        line=p.stdout.readline() if ready else b''
        (self.dest/(name+'.stdout')).write_bytes(line)
        return p,line
    @staticmethod
    def stop(p):
        if p.poll() is None:p.kill();p.wait(timeout=4)
        p.stdout.close()
    def submit(self):
        (self.dest/'requests.jsonl').write_text(envelope(payload(int(time.time()*1000)))+'\n')
    def terminal(self):
        return [r for r in rows(self.dest/'journal.jsonl') if r['kind'] in ('terminal','gate_reject')]
    def test_f02_concurrent_bridges_aliases_and_restart(self):
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            results=list(pool.map(lambda n:self.spawn('race%d'%n),range(2)))
        winners=[p for p,line in results if b'"ready": true' in line]
        self.assertEqual(len(winners),1)
        losers=[p for p,line in results if b'"ready": true' not in line]
        self.assertEqual(losers[0].wait(timeout=3),2)
        before=(self.dest/'journal.jsonl').read_bytes();reads=len(self.server.state['reads'])
        alias=self.dest/'alias';alias.symlink_to(self.dest/'journal.jsonl')
        p,line=self.spawn('alias',['--journal',str(alias)])
        self.assertEqual(p.wait(timeout=3),2);self.assertNotIn(b'"ready": true',line)
        self.assertEqual((self.dest/'journal.jsonl').read_bytes(),before)
        self.assertEqual(len(self.server.state['reads']),reads)
        self.stop(winners[0])  # before any intent: restart may perform the first send
        self.assertEqual(self.server.state['posts'],[])
        p,line=self.spawn('before-intent-restart');self.assertIn(b'"ready": true',line)
        self.submit();self.assertTrue(wait(self.terminal));self.assertEqual(self.terminal()[0]['event']['status'],'accepted')
        self.stop(p);p,line=self.spawn('restart')
        self.assertIn(b'"ready": true',line);time.sleep(.4)
        self.assertEqual(len(self.server.state['posts']),1)
        self.assertEqual(len(self.terminal()),1);self.stop(p)
        j=Journal(str(self.dest/'journal.jsonl'),lambda:int(time.time()*1000));j.open();j.close()
    def test_f04_missing_before_send(self):
        p,line=self.spawn('bridge');self.assertIn(b'"ready": true',line)
        self.server.state['mode']='missing-pre';self.submit();self.assertTrue(wait(self.terminal))
        self.assertEqual(self.terminal()[0]['event']['status'],'rejected');self.assertEqual(self.server.state['posts'],[])
    def test_f04_bad_shape_after_send(self):
        self.post_case('bad-post')
    def test_f04_empty_health_after_send(self):
        self.post_case('thin-post')
    def post_case(self,mode):
        self.server.state['mode']=mode;p,line=self.spawn('bridge');self.assertIn(b'"ready": true',line)
        self.submit();self.assertTrue(wait(self.terminal))
        self.assertEqual(self.terminal()[0]['event']['status'],'unknown');self.assertIsNone(p.poll())
        self.stop(p)
        # Restore a healthy schema: a completed unknown must still never upgrade/resend.
        self.server.state['mode']='good';p,line=self.spawn('restart');self.assertIn(b'"ready": true',line)
        time.sleep(.4);self.assertEqual(len(self.server.state['posts']),1)
        self.assertEqual(len(self.terminal()),1);self.assertEqual(self.terminal()[0]['event']['status'],'unknown')

if __name__=='__main__':unittest.main()
