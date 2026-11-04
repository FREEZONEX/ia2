"""P1 F10 without waiting: the client runs on a fake monotonic clock and a fake socket whose every operation costs scripted time and honours
settimeout() like a real one.  That makes the budget arithmetic exact -- one budget for connect, send, head, body and chunk metadata together,
every operation armed with what is left, an operation may finish AT the deadline but nothing may start after it -- and it lets the size
limits be read off the number of bytes the client consumed.  New-client tests only (they use the clock/connect parameters)."""
import os
import socket
import sys
import unittest
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from bridge import ia2client  # noqa: E402
from bridge.ia2client import Ia2Client, MAX_BODY, MAX_HEAD, MAX_META, READ_BLOCK  # noqa: E402

BODY = b'{"ok":true,"name":"v","value":5,"undelivered_device":null}'


def head(length=None, chunked=False):
    h = b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n'
    h += b'Transfer-Encoding: chunked\r\n' if chunked else (b'Content-Length: %d\r\n' % length if length is not None else b'')
    return h + b'\r\n'


class Clock:
    def __init__(self):
        self.t = 100.0

    def __call__(self):
        return self.t


class Sock:
    """`incoming` = [(seconds until these bytes arrive, bytes)]; sending costs `send_cost`.  settimeout() is honoured as a real socket does it:
    an operation that needs more than the timeout raises socket.timeout after the timeout has passed; one that needs exactly that long succeeds."""

    def __init__(self, clock, incoming, send_cost=0.0):
        self.clock, self.incoming, self.send_cost = clock, list(incoming), send_cost
        self.timeout, self.armed, self.sent, self.delivered, self.closed = None, [], b'', 0, False

    def settimeout(self, t):
        self.timeout = t
        self.armed.append((self.clock.t, t))

    def _spend(self, cost):
        if self.timeout is not None and cost > self.timeout:
            self.clock.t += self.timeout
            raise socket.timeout('timed out')
        self.clock.t += cost

    def sendall(self, data):
        self._spend(self.send_cost)
        self.sent += data

    def recv(self, n):
        if not self.incoming:
            return b''
        delay, data = self.incoming[0]
        self._spend(delay)
        if len(data) > n:
            self.incoming[0] = (0.0, data[n:])
            data = data[:n]
        else:
            self.incoming.pop(0)
        self.delivered += len(data)
        return data

    def close(self):
        self.closed = True


def run(incoming, budget=1.0, connect_cost=0.0, send_cost=0.0, method='GET', clock=None):
    clock = clock or Clock()
    sock = Sock(clock, incoming, send_cost)
    calls = []

    def connect(host, port, deadline):
        calls.append(clock.t)
        if connect_cost > deadline.left():
            clock.t += deadline.left()
            raise socket.timeout('connect timed out')
        clock.t += connect_cost
        return sock
    client = Ia2Client('127.0.0.1', 1, timeout_s=budget, clock=clock, connect=connect)
    t0 = clock.t
    rec = client.request(method, '/x', {'name': 'v', 'value': 5} if method == 'POST' else None)
    return rec, sock, clock.t - t0, calls


class SharedBudget(unittest.TestCase):
    def kind(self, rec):
        return (rec['kind'], rec.get('error_kind'))

    def test_the_phases_add_up_to_one_budget(self):
        # (binary fractions, so that "exactly the budget" is exact)  connect 3/8 + send 1/4 + head 1/4 + body 1/4 = 1.125: every phase is
        # well inside 1.0, together they are not; with a body of 1/8 the total is exactly 1.0
        script = [(0.25, head(len(BODY))), (0.25, BODY)]
        rec, sock, took, _ = run(script, connect_cost=0.375, send_cost=0.25)
        self.assertEqual(self.kind(rec), ('error', 'timeout'))
        self.assertAlmostEqual(took, 1.0, places=6, msg='it gives up exactly when the budget is used up')
        rec, _, took, _ = run([(0.25, head(len(BODY))), (0.125, BODY)], connect_cost=0.375, send_cost=0.25)
        self.assertEqual(rec['kind'], 'response')
        self.assertAlmostEqual(took, 1.0, places=6)

    def test_every_phase_alone_can_be_the_one_that_runs_out(self):
        cases = {
            'connect': dict(incoming=[(0.0, head(len(BODY)) + BODY)], connect_cost=1.5),
            'send': dict(incoming=[(0.0, head(len(BODY)) + BODY)], send_cost=1.5),
            'head': dict(incoming=[(1.5, head(len(BODY)) + BODY)]),
            'body': dict(incoming=[(0.0, head(len(BODY))), (1.5, BODY)]),
            'chunk size line': dict(incoming=[(0.0, head(chunked=True)), (1.5, b'5\r\nhello\r\n0\r\n\r\n')]),
            'chunk data': dict(incoming=[(0.0, head(chunked=True) + b'5\r\n'), (1.5, b'hello\r\n0\r\n\r\n')]),
            'trailer': dict(incoming=[(0.0, head(chunked=True) + b'5\r\nhello\r\n0\r\n'), (1.5, b'\r\n')]),
            'body until close': dict(incoming=[(0.0, head() + BODY), (1.5, b'')]),
        }
        for name, kw in cases.items():
            with self.subTest(phase=name):
                rec, sock, took, _ = run(**kw)
                self.assertEqual(self.kind(rec), ('error', 'timeout'))
                self.assertAlmostEqual(took, 1.0, places=6)
                if name == 'connect':
                    self.assertEqual(sock.sent, b'', 'no connection, so nothing was written')
                else:
                    self.assertTrue(sock.closed)

    def test_an_operation_may_finish_at_the_deadline_but_nothing_may_start_after_it(self):
        rec, sock, took, _ = run([(0.5, head(len(BODY))), (0.5, BODY)])             # the last byte arrives at exactly t = 1.0
        self.assertEqual(rec['kind'], 'response')
        self.assertAlmostEqual(took, 1.0, places=6)
        rec, sock, took, _ = run([(0.5, head(len(BODY))), (0.5, BODY[:-1]), (0.0, BODY[-1:])])      # one more receive is needed after t = 1.0
        self.assertEqual(self.kind(rec), ('error', 'timeout'))
        self.assertAlmostEqual(took, 1.0, places=6)

    def test_every_operation_is_armed_with_what_is_left(self):
        rec, sock, took, _ = run([(0.1, head(len(BODY))), (0.1, BODY[:20]), (0.1, BODY[20:])], budget=2.0, connect_cost=0.2, send_cost=0.1)
        self.assertEqual(rec['kind'], 'response')
        self.assertGreaterEqual(len(sock.armed), 4)                                 # the send and each receive
        end = 100.0 + 2.0
        for at, timeout in sock.armed:
            self.assertLessEqual(at + timeout, end + 1e-9, 'an operation was given more time than the budget has left')
            self.assertGreater(timeout, 0)
        self.assertEqual([round(t, 6) for _, t in sock.armed][:2], [round(2.0 - 0.2, 6), round(2.0 - 0.3, 6)])

    def test_a_pending_wall_clock_jump_changes_nothing(self):
        jumps = iter([10 ** 12, 0, 10 ** 14, 5, 10 ** 12 + 7, 3] * 10)
        with mock.patch.object(ia2client, '_now_ms', side_effect=lambda: next(jumps)):
            rec, _, took, _ = run([(0.25, head(len(BODY))), (0.125, BODY)], connect_cost=0.375, send_cost=0.25)
            self.assertEqual(rec['kind'], 'response')
            self.assertLess(rec['t_recv_ms'], rec['t_send_ms'] + 10 ** 13)            # the record shows whatever the wall clock said
            rec, _, took, _ = run([(0.25, head(len(BODY))), (0.25, BODY)], connect_cost=0.375, send_cost=0.25)
            self.assertEqual(self.kind(rec), ('error', 'timeout'))
            self.assertAlmostEqual(took, 1.0, places=6)

    def test_a_timeout_is_final_nothing_is_retried_a_post_is_sent_once(self):
        for method in ('GET', 'POST'):
            with self.subTest(method=method):
                rec, sock, _, connects = run([(5.0, head(len(BODY)) + BODY)], method=method)
                self.assertEqual(self.kind(rec), ('error', 'timeout'))
                self.assertEqual(len(connects), 1, 'no second connection')
                self.assertEqual(sock.sent.count(b'HTTP/1.1\r\n'), 1, 'the request was written once')
                self.assertTrue(sock.closed)

    def test_the_socket_is_closed_whatever_happens(self):
        for name, script in {'complete': [(0.0, head(len(BODY)) + BODY)], 'not http': [(0.0, b'garbage\r\n\r\n')], 'cut off': [(0.0, head(99) + BODY)],
                             'refused to answer': []}.items():
            with self.subTest(case=name):
                _, sock, _, _ = run(script)
                self.assertTrue(sock.closed)


class Connect(unittest.TestCase):
    def raises(self, kind, deadline):
        """connect_tcp must raise `kind`; any other outcome is a failed assertion, not an error."""
        try:
            ia2client.connect_tcp('h', 1, deadline)
        except kind:
            return
        except Exception as exc:  # noqa: BLE001
            self.fail('connect_tcp raised %r instead of %s' % (exc, kind.__name__))
        self.fail('connect_tcp did not raise %s' % kind.__name__)

    def fake_socket(self, clock, armed, refuse=()):
        class FakeSocket:
            def __init__(self, family, kind, proto):
                self.family = family

            def settimeout(self, t):
                armed.append(round(t, 6))

            def connect(self, addr):
                clock.t += 0.125
                if addr in refuse:
                    raise ConnectionRefusedError()

            def setsockopt(self, *a):
                pass

            def close(self):
                pass
        return FakeSocket

    def test_connect_is_armed_with_what_is_left_and_falls_through_the_addresses(self):
        clock, armed = Clock(), []
        deadline = ia2client.Deadline(1.0, clock)
        clock.t += 0.25
        two = [(10, 1, 6, '', ('::1', 1)), (2, 1, 6, '', ('127.0.0.1', 1))]
        with mock.patch.object(ia2client.socket, 'getaddrinfo', return_value=two), \
                mock.patch.object(ia2client.socket, 'socket', self.fake_socket(clock, armed, refuse=[('::1', 1)])):
            sock = ia2client.connect_tcp('localhost', 1, deadline)
        self.assertEqual(armed, [0.75, 0.625], 'each attempt gets what is left, not the whole budget')
        self.assertEqual(sock.family, 2)

    def test_all_addresses_refused_is_the_last_error_and_a_spent_budget_is_a_timeout(self):
        clock, armed = Clock(), []
        two = [(2, 1, 6, '', ('127.0.0.1', 1)), (2, 1, 6, '', ('127.0.0.2', 1))]
        with mock.patch.object(ia2client.socket, 'getaddrinfo', return_value=two), \
                mock.patch.object(ia2client.socket, 'socket', self.fake_socket(clock, armed, refuse=[a[4] for a in two])):
            self.raises(ConnectionRefusedError, ia2client.Deadline(1.0, clock))
            self.raises(socket.timeout, ia2client.Deadline(-1.0, clock))            # a budget that is already spent

    def test_the_default_clock_is_the_monotonic_one(self):
        import time
        self.assertIs(Ia2Client('127.0.0.1', 1).clock, time.monotonic)
        self.assertIs(Ia2Client('127.0.0.1', 1).connect, ia2client.connect_tcp)


class Bounds(unittest.TestCase):
    def over(self, script, bound, what):
        rec, sock, _, _ = run(script, budget=60.0)
        self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'protocol'), rec.get('error_text'))
        self.assertLessEqual(sock.delivered, bound, '%s: the client kept reading past its limit (%d bytes consumed)' % (what, sock.delivered))

    def test_the_body_stops_the_client_a_block_after_its_limit(self):
        h = head()
        self.over([(0.0, h)] + [(0.0, b'x' * 65536)] * 100, len(h) + MAX_BODY + READ_BLOCK, 'no length')
        h = head(chunked=True)
        self.over([(0.0, h)] + [(0.0, b'10000\r\n' + b'x' * 0x10000 + b'\r\n')] * 100, len(h) + MAX_BODY + 3 * READ_BLOCK, 'many chunks')
        self.over([(0.0, h), (0.0, b'%x\r\n' % (MAX_BODY + 1))] + [(0.0, b'x' * 65536)] * 100, len(h) + 16, 'one chunk that announces too much')

    def test_the_head_stops_the_client_a_block_after_its_limit(self):
        self.over([(0.0, b'HTTP/1.1 200 OK\r\nX: ' + b'a' * 100)] + [(0.0, b'a' * 65536)] * 100, MAX_HEAD + 2 * READ_BLOCK, 'a head that never ends')
        self.over([(0.0, b'HTTP/1.1 200 OK\r\n' + b'H: v\r\n' * 1000 + b'\r\n')], MAX_HEAD + 2 * READ_BLOCK, 'a head of many headers')
        self.over([(0.0, b'HTTP/1.1 100 Continue\r\n\r\n' * 30)], 30 * 26, 'interim answers without end')

    def test_chunk_metadata_has_its_own_limit_apart_from_the_body(self):
        h = head(chunked=True)
        self.over([(0.0, h), (0.0, b'5;' + b'a' * 100)] + [(0.0, b'a' * 65536)] * 100, len(h) + MAX_META + 2 * READ_BLOCK, 'a chunk-size line that never ends')
        self.over([(0.0, h)] + [(0.0, b'1\r\nx\r\n' * 2000)] * 100, len(h) + 3 * MAX_META, 'chunks of one byte, for ever')
        self.over([(0.0, h), (0.0, b'0\r\n')] + [(0.0, b'T: ' + b'a' * 1000 + b'\r\n')] * 100, len(h) + 3 * MAX_META, 'trailers, for ever')

    def test_metadata_is_not_charged_to_the_body_and_the_body_not_to_the_metadata(self):
        # 3 chunks of 0.9 MiB with 16-byte size lines: body 2.7 MiB is over the BODY limit, metadata is tiny; and the reverse: a body of
        # 4000 one-byte chunks is under both limits (metadata 12 KB, body 4 KB)
        h = head(chunked=True)
        rec, _, _, _ = run([(0.0, h)] + [(0.0, b'e6666\r\n' + b'x' * 0xe6666 + b'\r\n')] * 3, budget=60.0)
        self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'protocol'))
        rec, _, _, _ = run([(0.0, h + b'1\r\nx\r\n' * 4000 + b'0\r\n\r\n')], budget=60.0)
        self.assertEqual((rec['kind'], len(rec['text'])), ('response', 4000))


if __name__ == '__main__':
    unittest.main()
