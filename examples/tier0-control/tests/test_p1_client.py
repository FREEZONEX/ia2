"""P1 F10, on real loopback sockets: one HTTP operation has one budget and bounded sizes, whichever phase the peer is slow in.

The peers here are finite throw-away servers (one connection, a script of sends and sleeps).  Time is measured with the monotonic clock
and judged with a fixed local scheduling margin of 100 ms; the margin is never widened to make a run pass.  A timeout means "unknown":
these tests only look at what the client returns and how long it takes."""
import os
import socket
import sys
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from bridge.ia2client import Ia2Client, MAX_BODY  # noqa: E402

MARGIN = 0.1                  # seconds of scheduling slack allowed on top of a budget
OK_BODY = b'{"ok":true,"name":"v","value":5,"undelivered_device":null}'


def head(length=None, extra=b'', chunked=False, status=b'200 OK'):
    h = b'HTTP/1.1 ' + status + b'\r\nContent-Type: application/json\r\nConnection: close\r\n' + extra
    if chunked:
        h += b'Transfer-Encoding: chunked\r\n'
    elif length is not None:
        h += b'Content-Length: %d\r\n' % length
    return h + b'\r\n'


def trickle(data, delay):
    """One byte at a time, `delay` seconds apart."""
    out = []
    for i in range(len(data)):
        out += [('send', data[i:i + 1]), ('sleep', delay)]
    return out


class Peer:
    """Accepts one connection, reads the request, plays `script` ([('send', bytes) | ('sleep', s)]), then closes."""

    def __init__(self, script):
        self.srv = socket.socket()
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind(('127.0.0.1', 0))
        self.srv.listen(1)
        self.port = self.srv.getsockname()[1]
        self.script, self.request, self.gone = script, b'', False
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        conn, _ = self.srv.accept()
        conn.settimeout(5)
        try:
            data = b''
            while b'\r\n\r\n' not in data:
                more = conn.recv(65536)
                if not more:                       # the client closed before it finished (or began) its request
                    self.request = data
                    return
                data += more
            hdr, _, body = data.partition(b'\r\n\r\n')
            want = [int(l.split(b':')[1]) for l in hdr.split(b'\r\n') if l.lower().startswith(b'content-length')]
            while want and len(body) < want[0]:
                more = conn.recv(65536)
                if not more:
                    break
                body += more
            self.request = hdr + b'\r\n\r\n' + body
            for kind, arg in self.script:
                if kind == 'send':
                    try:
                        conn.sendall(arg)
                    except OSError:
                        self.gone = True
                        return
                else:
                    time.sleep(arg)
        except OSError:
            pass
        finally:
            conn.close()
            self.srv.close()


def call(script, budget=None, method='GET', **kw):
    peer = Peer(script)
    client = Ia2Client('127.0.0.1', peer.port, timeout_s=budget or 3.0, get_timeout_s=budget or 2.0)
    t0 = time.monotonic()
    if method == 'GET':
        rec = client.request('GET', '/x', **kw)
    else:
        rec = client.request('POST', '/write', {'name': 'v', 'value': 5}, origin='tier0.t.00000000')
    return rec, time.monotonic() - t0, peer


class Budget(unittest.TestCase):
    def assertTimedOut(self, rec, took, budget):
        self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'timeout'), rec.get('text', '')[:60])
        self.assertLess(took, budget + MARGIN, 'the call outlived its budget by more than the scheduling margin')
        self.assertGreaterEqual(took, budget * 0.9, 'it must use the budget, not give up early')

    def test_F10_01_a_peer_answering_a_byte_at_a_time_cannot_outlast_the_budget(self):
        script = [('send', head(12))] + trickle(b'x' * 12, 0.06)               # 12 bytes, one per 60 ms: 720 ms in all
        for method in ('GET', 'POST'):
            with self.subTest(method=method):
                rec, took, _ = call(script, budget=0.15, method=method)
                self.assertTimedOut(rec, took, 0.15)

    def test_F10_01b_get_json_has_the_same_bound(self):
        peer = Peer([('send', head(12))] + trickle(b'x' * 12, 0.06))
        client = Ia2Client('127.0.0.1', peer.port, get_timeout_s=0.15)
        t0 = time.monotonic()
        parsed, rec = client.get_json('/status')
        self.assertIsNone(parsed)
        self.assertTimedOut(rec, time.monotonic() - t0, 0.15)

    def test_F10_02_every_phase_can_be_the_slow_one(self):
        body = OK_BODY
        slow = {
            'head delayed': [('sleep', 1.0), ('send', head(len(body)) + body)],
            'head trickled': trickle(head(len(body)), 0.03) + [('send', body)],
            'body delayed': [('send', head(len(body))), ('sleep', 1.0), ('send', body)],
            'body trickled': [('send', head(len(body)))] + trickle(body, 0.05),
            'no length, body trickled': [('send', head())] + trickle(body[:12], 0.06),
            'no length, never closes in time': [('send', head() + body), ('sleep', 1.0)],
            'chunk-size line delayed': [('send', head(chunked=True)), ('sleep', 1.0), ('send', b'5\r\nhello\r\n0\r\n\r\n')],
            'chunk size trickled': [('send', head(chunked=True))] + trickle(b'5\r\nhello\r\n0\r\n\r\n', 0.05),
            'chunk data trickled': [('send', head(chunked=True) + b'c\r\n')] + trickle(b'x' * 12, 0.06),
            'last chunk without its blank line': [('send', head(chunked=True) + b'5\r\nhello\r\n0\r\n'), ('sleep', 1.0)],
        }
        for name, script in slow.items():
            with self.subTest(phase=name):
                rec, took, _ = call(script, budget=0.2)
                self.assertTimedOut(rec, took, 0.2)

    def test_F10_03_an_answer_that_ends_inside_the_budget_is_not_cut(self):
        pieces = [OK_BODY[i:i + 11] for i in range(0, len(OK_BODY), 11)]
        script = [('send', head(len(OK_BODY)))]
        for piece in pieces:
            script += [('send', piece), ('sleep', 0.02)]
        rec, took, _ = call(script, budget=1.0)
        self.assertEqual((rec['kind'], rec['status'], rec['json']['value']), ('response', 200, 5))
        self.assertLess(took, 1.0)

    def test_F10_04_the_three_ways_to_frame_a_body_all_complete(self):
        chunked = b'%x\r\n%s\r\n%x\r\n%s\r\n0\r\n\r\n' % (20, OK_BODY[:20], len(OK_BODY) - 20, OK_BODY[20:])
        for name, script in {'content-length': [('send', head(len(OK_BODY)) + OK_BODY)],
                             'chunked': [('send', head(chunked=True) + chunked)],
                             'chunked with an extension and a trailer': [('send', head(chunked=True) + b'%x;ext=1\r\n%s\r\n0\r\nX-T: 1\r\n\r\n' % (len(OK_BODY), OK_BODY))],
                             'chunked, peer closes after the last chunk': [('send', head(chunked=True) + b'%x\r\n%s\r\n0\r\n' % (len(OK_BODY), OK_BODY))],
                             'until the peer closes': [('send', head() + OK_BODY)]}.items():
            with self.subTest(framing=name):
                rec, _, _ = call(script, budget=1.0)
                self.assertEqual((rec['kind'], rec.get('status'), rec.get('text')), ('response', 200, OK_BODY.decode()), rec.get('error_kind'))

    def test_F10_05_short_cut_and_broken_answers(self):
        cases = {
            'body shorter than promised': ([('send', head(80) + OK_BODY)], 'truncated'),
            'chunk shorter than promised': ([('send', head(chunked=True) + b'a\r\nabcd')], 'truncated'),
            'closed before the head ends': ([('send', b'HTTP/1.1 200 OK\r\nContent-Le')], 'truncated'),
            'closed without an answer': ([], 'reset'),
            'not an HTTP status line': ([('send', b'SSH-2.0-nothing\r\n\r\n')], 'protocol'),
            'unsupported transfer coding': ([('send', head(extra=b'Transfer-Encoding: gzip, chunked\r\n') + b'0\r\n\r\n')], 'protocol'),
            'bad chunk size': ([('send', head(chunked=True) + b'zz\r\nhello\r\n')], 'protocol'),
            'bad chunk terminator': ([('send', head(chunked=True) + b'5\r\nhelloXX0\r\n\r\n')], 'protocol'),
            'declared length over the limit': ([('send', head(MAX_BODY + 1))], 'protocol'),
            'declared length is not a number': ([('send', head(extra=b'Content-Length: 1e3\r\n') + b'x')], 'protocol'),
        }
        for name, (script, kind) in cases.items():
            with self.subTest(case=name):
                rec, took, _ = call(script, budget=1.0)
                self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', kind), rec.get('error_text'))
                self.assertLess(took, 0.6, 'a broken answer is reported when it is seen, not when the budget ends')

    def test_F10_06_sizes_are_bounded_while_the_bytes_arrive(self):
        big = b'x' * (MAX_BODY + 1)
        cases = {
            'no length, one byte over the body limit': [('send', head() + big)],
            'chunked, one chunk over the body limit': [('send', head(chunked=True) + b'%x\r\n' % (MAX_BODY + 1))],
            'chunked, chunks add up over the body limit': [('send', head(chunked=True) + (b'80000\r\n' + b'x' * 0x80000 + b'\r\n') * 5)],
            'head larger than its limit': [('send', head(extra=b'X-Pad: ' + b'a' * 20000 + b'\r\n') + b'x')],
            'more headers than allowed': [('send', head(extra=b''.join(b'H%d: v\r\n' % i for i in range(150))) + b'x')],
            'chunk extension larger than the metadata limit': [('send', head(chunked=True) + b'5;' + b'a' * 20000 + b'\r\nhello\r\n0\r\n\r\n')],
            'trailer larger than the metadata limit': [('send', head(chunked=True) + b'0\r\n' + b'T: ' + b'a' * 20000 + b'\r\n\r\n')],
            'endless tiny chunks': [('send', head(chunked=True))] + [('send', b'1\r\nx\r\n' * 1000)] * 40,
        }
        for name, script in cases.items():
            with self.subTest(case=name):
                rec, took, _ = call(script, budget=2.0)
                self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'protocol'), (rec.get('error_kind'), rec.get('text', '')[:40]))
                self.assertLess(took, 1.0)

    def test_F10_07_bodies_exactly_at_the_limit_complete(self):
        exact = b'x' * MAX_BODY
        chunked = (b'80000\r\n' + b'x' * 0x80000 + b'\r\n') * 4 + b'0\r\n\r\n'
        for name, script in {'content-length': [('send', head(MAX_BODY) + exact)], 'no length': [('send', head() + exact)],
                             'chunked': [('send', head(chunked=True) + chunked)]}.items():
            with self.subTest(framing=name):
                rec, _, _ = call(script, budget=5.0)
                self.assertEqual((rec['kind'], len(rec.get('text', ''))), ('response', MAX_BODY))

    def test_F10_08_the_request_itself_is_what_it_was(self):
        rec, _, peer = call([('send', head(len(OK_BODY)) + OK_BODY)], budget=1.0, method='POST')
        self.assertEqual(rec['kind'], 'response')
        req = peer.request
        self.assertTrue(req.startswith(b'POST /write HTTP/1.1\r\n'))
        self.assertIn(b'X-IA2-Origin: tier0.t.00000000\r\n', req)
        self.assertIn(b'Content-Type: application/json\r\n', req)
        sent = b'{"name":"v","value":5}'
        self.assertTrue(req.endswith(b'\r\n\r\n' + sent))
        self.assertIn(b'Content-Length: %d\r\n' % len(sent), req)

    def test_F10_09_a_request_that_cannot_be_written_safely_is_an_error_not_an_exception(self):
        peer = Peer([])
        try:
            rec = Ia2Client('127.0.0.1', peer.port).request('POST', '/write', {'name': 'v', 'value': 1}, origin='bad\r\nX-Evil: 1')
        except Exception as exc:  # noqa: BLE001
            self.fail('request() raised %r instead of returning an error record' % (exc,))
        self.assertEqual((rec['kind'], rec['error_kind']), ('error', 'protocol'))
        peer.thread.join(timeout=2)
        self.assertEqual(peer.request, b'', 'nothing of the request may have been written')


if __name__ == '__main__':
    unittest.main()
