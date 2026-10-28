"""F02: lifetime inode lock; real processes, no network."""
import contextlib
import io
import json
from unittest.mock import patch
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import SRC
from bridge.journal import Journal, JournalError

CHILD = '''import sys
sys.dont_write_bytecode=True
sys.path.insert(0,sys.argv[1])
from bridge.journal import Journal,JournalError
j=Journal(sys.argv[2],lambda:1000)
try: j.open()
except JournalError:
 print('busy',flush=True);sys.exit(2)
print('owned',flush=True)
sys.stdin.readline()
j.append('start')
j.close()
'''

class Lock(unittest.TestCase):
    def child(self, path):
        p = subprocess.Popen([sys.executable,'-I','-c',CHILD,str(SRC),str(path)],
                             stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        self.addCleanup(self.finish,p)
        return p

    @staticmethod
    def finish(p):
        if p.poll() is None: p.kill()
        p.communicate(timeout=5)

    def test_simultaneous_competitors_and_killed_owner(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'j'
            ps=[self.child(path) for _ in range(6)]
            results=[p.stdout.readline().strip() for p in ps]
            self.assertEqual(results.count('owned'),1)
            self.assertEqual(results.count('busy'),5)
            self.assertEqual(path.read_bytes(),b'')
            owner=ps[results.index('owned')]
            owner.kill();owner.wait(timeout=5)
            j=Journal(str(path),lambda:1000);j.open();j.append('start');j.close()
            j=Journal(str(path),lambda:1000);j.open()
            self.assertEqual(j.seq,1);j.close()

    def test_aliases_cannot_replay_or_repair_and_normal_exit_releases(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'j'
            p=self.child(path);self.assertEqual(p.stdout.readline().strip(),'owned')
            # A denied opener must not even trim this incomplete suffix.
            path.write_bytes(b'partial')
            symlink=Path(tmp)/'sym';symlink.symlink_to(path)
            hardlink=Path(tmp)/'hard';os.link(path,hardlink)
            for alias in (path,symlink,hardlink,path.parent/'.'/path.name):
                q=self.child(alias)
                self.assertEqual(q.stdout.readline().strip(),'busy')
                self.assertEqual(q.wait(timeout=5),2)
                self.assertEqual(path.read_bytes(),b'partial')
            path.write_bytes(b'')
            p.communicate('\n',timeout=5)
            j=Journal(str(symlink),lambda:1000);j.open()
            self.assertEqual(j.seq,1);j.close()

    def test_startup_failure_releases_lock_in_same_process(self):
        from bridge import main
        from p0_support import KEYS, reg_for
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'j'
            args=['--registry','unused','--registry-sha256','unused','--keys','unused',
                  '--runtime','127.0.0.1:1','--journal',str(path),'--trace',str(Path(tmp)/'t'),
                  '--requests',str(Path(tmp)/'q'),'--project-dir',tmp,'--governance-check','off']
            with patch.object(main.auth,'load_keys',return_value=KEYS), \
                 patch.object(main.registry_mod,'load_pinned',return_value=reg_for()), \
                 patch.object(main.northbound_guard,'check',return_value='absent'), \
                 patch.object(main.preflight,'check',return_value={'reachable':True,'problems':['test missing field']}), \
                 contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(main.main(args),4)
            j=Journal(str(path),lambda:1000);j.open();j.close()

    def test_failed_replay_releases_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/'j';path.write_bytes(b'bad\n')
            for _ in range(2):
                with self.assertRaisesRegex(JournalError,'not valid JSON'):
                    Journal(str(path),lambda:1000).open()
            path.write_bytes(b'')
            j=Journal(str(path),lambda:1000);j.open();j.close()

if __name__=='__main__': unittest.main()
