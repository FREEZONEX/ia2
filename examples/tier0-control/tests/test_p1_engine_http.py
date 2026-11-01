"""P1 F10 at the engine: the real client against a loopback stand-in for IA2 whose answers can be made slow.

  * a slow POST that the server has already recorded is `unknown` after its budget, and is never sent again (replay, restart, reconcile);
  * a slow GET before the send costs that one action (rejected, 0 POST) at most the GET budget, and the next action goes through;
  * post-send reads that stall cost no more than their own budgets.

Budgets are shortened (GET 0.3 s, POST 0.4 s) and judged with the same fixed 100 ms local margin as the client tests.  Everything is on loopback."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path
import json
import socketserver
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, KEYS, envelope, payload, reg_for
from bridge.engine import Engine, now_ms
from bridge.ia2client import Ia2Client
from bridge.journal import Journal
from signing_fixtures import TraceWriter

GET_S, POST_S, MARGIN = 0.3, 0.4, 0.1
AID2 = 'b' * 32


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        st, conn = self.server.state, self.request
        conn.settimeout(5)
        data = b''
        while b'\r\n\r\n' not in data:
            more = conn.recv(65536)
            if not more:
                return
            data += more
        hdr, _, body = data.partition(b'\r\n\r\n')
        method, path, _ = hdr.split(b'\r\n')[0].decode().split(' ', 2)
        want = [int(l.split(b':')[1]) for l in hdr.split(b'\r\n') if l.lower().startswith(b'content-length')]
        while want and len(body) < want[0]:
            body += conn.recv(65536)
        n = time.monotonic_ns() // 1000000
        mode = st['modes'].get(path, 'good')
        if method == 'POST':
            req = json.loads(body)
            st['posts'].append(dict(path=path, body=req, origin=_header(hdr, b'x-ia2-origin')))
            doc = dict(ok=True, name=req['name'], value=req['value'], undelivered_device=None)
        else:
            if mode == 'trickle-after-post' and not st['posts']:
                mode = 'good'
            st['reads'].append(path)
            if path == '/status':
                doc = dict(runtime_id='R', scan_count=n, mode={'kind': 'running'}, forces=[], watchdog_tripped=False, latch_reason=None, fault=None,
                           device_health=[dict(name='dev', healthy=True)], last_snapshot=dict(timestamp_us=n * 1000, vars=[]))
            elif path == '/health':
                doc = dict(status='ok', fault=None, latch_reason=None, watchdog_tripped=False, fieldbus_healthy=True, snapshot_tap={'running': True})
            else:
                doc = []
        raw = json.dumps(doc).encode()
        head = b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n' % len(raw)
        if mode.startswith('trickle'):
            conn.sendall(head)
            for i in range(len(raw)):                       # a byte every 100 ms: far longer than any budget here
                time.sleep(0.1)
                try:
                    conn.sendall(raw[i:i + 1])
                except OSError:
                    return
        else:
            conn.sendall(head + raw)


def _header(hdr, name):
    for line in hdr.split(b'\r\n')[1:]:
        k, _, v = line.partition(b':')
        if k.strip().lower() == name:
            return v.strip().decode()


class Ia2Stand(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


class EngineHttp(unittest.TestCase):
    def setUp(self):
        self.server = Ia2Stand(('127.0.0.1', 0), Handler)
        self.server.state = dict(posts=[], reads=[], modes={})
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.tmp = tempfile.TemporaryDirectory()
        self.engines = []

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        for e in self.engines:
            e.journal.close()
            e.trace.close()
        self.tmp.cleanup()

    def engine(self, sub=''):
        """A bridge engine on the journal in <tmp>/<sub>; a different `sub` is a different bridge with its own journal."""
        directory = Path(self.tmp.name) / sub
        directory.mkdir(exist_ok=True)
        jr = Journal(str(directory / 'journal.jsonl'), now_ms)
        jr.open()
        e = Engine(dict(poll_ms=10, advance_wait_ms=600, hold_path=None, project_dir=None), reg_for(), KEYS, jr,
                   Ia2Client('127.0.0.1', self.server.server_address[1], timeout_s=POST_S, get_timeout_s=GET_S),
                   TraceWriter(str(directory / 'trace.jsonl')))
        self.engines.append(e)
        return e

    def close(self, e):
        e.journal.close()
        e.trace.close()
        self.engines.remove(e)

    @property
    def posts(self):
        return self.server.state['posts']

    def line(self, aid=AID):
        return envelope(payload(now_ms(), action_id=aid))

    def test_F10_a_slow_post_the_server_recorded_is_unknown_and_never_sent_again(self):
        self.server.state['modes']['/write'] = 'trickle'
        e = self.engine()
        line = self.line()
        t0 = time.monotonic()
        e.handle_line(line)
        took = time.monotonic() - t0
        ev = e.journal.final_event(AID)
        self.assertEqual((ev['status'], ev['reason'], ev.get('detail')), ('unknown', 'runtime_unreachable', 'timeout'))
        self.assertLess(took, POST_S + 3 * 0.02 + MARGIN + 0.05 * 3, 'the pre-send reads are fast; the POST costs its budget and no more')
        self.assertGreaterEqual(took, POST_S * 0.9)
        self.assertEqual(len(self.posts), 1)
        rec = [json.loads(l) for l in (Path(self.tmp.name) / 'journal.jsonl').read_text().splitlines()]
        self.assertEqual([r['kind'] for r in rec if r.get('action_id') == AID], ['intent', 'response', 'post_check', 'terminal'])
        resp = next(r for r in rec if r['kind'] == 'response')
        self.assertEqual((resp['http']['kind'], resp['http']['error_kind']), ('error', 'timeout'))
        # the same request again, a restart, a second restart, the reconcile passes: still the one POST, still the one terminal
        e.handle_line(line)
        self.close(e)
        for _ in range(2):
            e = self.engine()
            e.recover()
            e.handle_line(line)
            e.reconcile_unknowns()
            self.assertEqual(e.journal.final_event(AID)['status'], 'unknown')
            self.close(e)
        self.assertEqual(len(self.posts), 1)
        self.assertEqual([json.loads(l)['kind'] for l in (Path(self.tmp.name) / 'journal.jsonl').read_text().splitlines()].count('terminal'), 1)

    def test_F10_b_a_slow_get_costs_one_action_at_most_the_get_budget_and_the_next_one_goes_through(self):
        self.server.state['modes']['/status'] = 'trickle'
        e = self.engine()
        t0 = time.monotonic()
        e.handle_line(self.line(AID))
        took = time.monotonic() - t0
        ev = e.journal.final_event(AID)
        self.assertEqual((ev['status'], ev['reason']), ('rejected', 'runtime_unreachable'))
        self.assertLess(took, GET_S + MARGIN)
        self.assertGreaterEqual(took, GET_S * 0.9)
        self.assertEqual(self.posts, [], 'a read that did not finish rejects the action before anything is sent')
        self.server.state['modes']['/status'] = 'good'
        e.handle_line(self.line(AID2))
        ev2 = e.journal.final_event(AID2)
        self.assertEqual((ev2['status'], ev2['reason']), ('accepted', None))
        self.assertEqual(len(self.posts), 1)
        self.assertEqual(self.posts[0]['origin'], 'tier0.review.' + AID2[:8])
        self.assertEqual(e.journal.final_event(AID)['status'], 'rejected', 'the first action keeps its answer')

    def test_F10_c_post_send_reads_that_stall_cost_their_own_budget_each(self):
        for route, want in (('/health', ('unknown', 'runtime_unreachable')), ('/audit', ('accepted', None))):
            with self.subTest(stalled=route):
                self.server.state.update(posts=[], reads=[], modes={route: 'trickle-after-post'})
                e = self.engine(sub=route.strip('/'))
                aid = ('c' if route == '/health' else 'd') * 32
                t0 = time.monotonic()
                e.handle_line(self.line(aid))
                took = time.monotonic() - t0
                ev = e.journal.final_event(aid)
                self.assertEqual((ev['status'], ev['reason']), want)
                self.assertLess(took, GET_S + MARGIN + 0.1, 'one stalled read of the post-check is worth one GET budget')
                self.assertEqual(len(self.posts), 1)


if __name__ == '__main__':
    unittest.main()
