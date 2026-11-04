"""P1 F07 with the real bridge process and a loopback HTTP stand-in for IA2.

  * the journal stops syncing in the middle of a request: the process ends with exit code 7 and a message that says why, it has sent exactly
    one POST, and the next start replays the journal, sends nothing again and serves the next request;
  * a status file that cannot be written is refused at start (exit code 2), not found out later;
  * a housekeeping step that fails after the start (here: the status file) does not end the service."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path, decode_event
import hashlib
import json
import os
import selectors
import shutil
import subprocess
import sys
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, KEYS, SRC, envelope, payload, reg_for
from bridge.journal import Journal

OUT = SRC / 'evidence' / 'process-p1'
AID2 = 'b' * 32
HOOKS = str(Path(__file__).resolve().parent / 'hooks_fail_sync.py')


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, body):
        raw = json.dumps(body).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        st, n = self.server.state, time.monotonic_ns() // 1000000
        st['reads'].append(self.path)
        if self.path == '/status':
            body = dict(runtime_id='fixture-R', scan_count=n, mode={'kind': 'running'}, forces=[], watchdog_tripped=False, latch_reason=None, fault=None,
                        device_health=[dict(name='dev', healthy=True)], last_snapshot=dict(timestamp_us=n * 1000, vars=[dict(name='sp', type_name='DINT', bits=0)]))
        elif self.path == '/health':
            body = dict(status='ok', fault=None, latch_reason=None, watchdog_tripped=False, fieldbus_healthy=True, snapshot_tap={'running': True})
        else:
            body = []
        self.reply(body)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.state['posts'].append(dict(body=body, origin=self.headers.get('X-IA2-Origin')))
        self.reply(dict(ok=True, name=body['name'], value=body['value'], undelivered_device=None))


def wait(fn, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if fn():
            return True
        time.sleep(0.02)
    return bool(fn())


def rows(path):
    out = []
    if Path(path).exists():
        for line in Path(path).read_text().splitlines():
            try:
                out.append(decode_event(json.loads(line)) if Path(path).name == 'trace.jsonl' else json.loads(line))
            except ValueError:
                pass
    return out


class Process(unittest.TestCase):
    def setUp(self):
        n = 1
        while (OUT / ('%s-%d' % (self._testMethodName, n))).exists():
            n += 1
        self.dest = OUT / ('%s-%d' % (self._testMethodName, n))
        self.dest.mkdir(parents=True)
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.state = dict(posts=[], reads=[])
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.procs = []
        text = json.dumps(dict(version=1, targets=list(reg_for().targets.values())))
        (self.dest / 'registry.json').write_text(text)
        (self.dest / 'keys.json').write_text(json.dumps({'k': dict(secret_hex=KEYS['k']['secret'].hex(), not_before_ms=0, not_after_ms=2 ** 60)}))
        (self.dest / 'keys.json').chmod(0o600)
        (self.dest / 'project.toml').write_text('[governance]\nwrite_mode = "allowlist"\n[[governance.rules]]\nvariable = "sp"\nmin = 0\nmax = 100\n')
        (self.dest / 'requests.jsonl').write_text('')
        self.cmd = [sys.executable, '-I', str(SRC / 'bridge_run.py'), '--registry', str(self.dest / 'registry.json'),
                    '--registry-sha256', hashlib.sha256(text.encode()).hexdigest(), '--keys', str(self.dest / 'keys.json'), '--event-keys', event_keys_path(str(self.dest / 'keys.json')),
                    '--runtime', '127.0.0.1:%d' % self.server.server_port, '--journal', str(self.dest / 'journal.jsonl'),
                    '--trace', str(self.dest / 'trace.jsonl'), '--requests', str(self.dest / 'requests.jsonl'), '--project-dir', str(self.dest)]

    def tearDown(self):
        for p in self.procs:
            if p.poll() is None:
                p.kill()
                p.wait(timeout=4)
            p.stdout.close()
        (self.dest / 'observed.json').write_text(json.dumps(self.server.state, indent=2) + '\n')
        self.server.shutdown()
        self.server.server_close()

    def spawn(self, name, extra=()):
        with open(self.dest / (name + '.stderr'), 'wb') as err:
            p = subprocess.Popen(self.cmd + list(extra), stdout=subprocess.PIPE, stderr=err)
        self.procs.append(p)
        sel = selectors.DefaultSelector()
        sel.register(p.stdout, selectors.EVENT_READ)
        ready = sel.select(8)
        sel.close()
        line = p.stdout.readline() if ready else b''
        return p, line

    def stderr(self, name):
        return (self.dest / (name + '.stderr')).read_text()

    def submit(self, aid, text=None):
        with open(self.dest / 'requests.jsonl', 'a') as f:
            f.write(envelope(payload(int(time.time() * 1000), action_id=aid)) + '\n')

    def ended(self, aid):
        return [r for r in rows(self.dest / 'journal.jsonl') if r['kind'] in ('terminal', 'gate_reject') and r.get('action_id') == aid]

    def posts_of(self, aid):
        return [p for p in self.server.state['posts'] if p['origin'].endswith(aid[:8])]

    def test_F07_12_a_journal_that_stops_syncing_ends_the_bridge_with_exit_code_7_and_nothing_is_sent_again(self):
        p, line = self.spawn('first', ['--test-hooks', HOOKS])
        self.assertIn(b'"ready": true', line)
        self.submit(AID)
        self.assertEqual(p.wait(timeout=10), 7)
        self.assertIn('journal failure', self.stderr('first'))
        self.assertIn('the disk went away', self.stderr('first'), 'the message names the cause')
        self.assertIn('Errno 5', self.stderr('first'))
        self.assertEqual(len(self.posts_of(AID)), 1)
        self.submit(AID2)                                              # a request that arrives while no bridge is running
        time.sleep(0.3)
        self.assertEqual(len(self.server.state['posts']), 1)
        p, line = self.spawn('second')                                 # the next start: the journal replays, nothing is sent again, the new request is served
        self.assertIn(b'"ready": true', line)
        self.assertTrue(wait(lambda: self.ended(AID2)))
        self.assertEqual(self.ended(AID2)[0]['event']['status'], 'accepted')
        self.assertEqual(len(self.ended(AID)), 1, 'the first action has exactly one result, concluded at the restart')
        self.assertEqual((len(self.posts_of(AID)), len(self.posts_of(AID2))), (1, 1))
        p.kill()
        p.wait(timeout=4)
        Journal(str(self.dest / 'journal.jsonl'), lambda: int(time.time() * 1000)).open()          # the chain is valid and the lock was released

    def test_F07_13_a_status_file_that_cannot_be_written_is_refused_at_start(self):
        p, line = self.spawn('refused', ['--status-file', str(self.dest / 'no-such-directory' / 'status.json')])
        try:
            code = p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.fail('the bridge started although its status file cannot be written')
        self.assertEqual(code, 2)
        self.assertEqual(line, b'')
        self.assertIn('status file', self.stderr('refused'))
        self.assertEqual(self.server.state['posts'], [])

    def test_F07_14_a_housekeeping_failure_after_the_start_does_not_end_the_service(self):
        p, line = self.spawn('bridge')
        self.assertIn(b'"ready": true', line)
        blocker = self.dest / 'journal.jsonl.status.json.tmp'          # the file write_status() writes first: now it is a directory, so every write fails
        for _ in range(200):
            try:
                blocker.mkdir()
                break
            except FileExistsError:                                    # the bridge happened to be writing it at that very moment
                time.sleep(0.01)
        self.assertTrue(blocker.is_dir())
        self.assertTrue(wait(lambda: 'housekeeping' in self.stderr('bridge'), timeout=6), 'the slow loop ran and reported the failure')
        self.assertIsNone(p.poll(), 'the bridge is still running')
        self.submit(AID)
        self.assertTrue(wait(lambda: self.ended(AID)))
        self.assertEqual(self.ended(AID)[0]['event']['status'], 'accepted')
        self.assertEqual(len(self.posts_of(AID)), 1)
        time.sleep(2.5)
        self.assertEqual(self.stderr('bridge').count("housekeeping step 'status file'"), 1, 'said once, not at every tick')
        self.assertIsNone(p.poll())


if __name__ == '__main__':
    unittest.main()
