"""IR01 regression: ambiguous framing must not supply successful write evidence. Offline memory sockets and the real engine/verifier."""
import json
import random
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, Client, close, engine, envelope, payload, reg_for
from bridge.ia2client import Ia2Client
import verifier

START = 1800000000000
BODY = b'{"ok":true,"name":"sp","value":1,"undelivered_device":null}'
N = len(BODY)
CHUNKS = b'%x\r\n' % N + BODY + b'\r\n0\r\n\r\n'


class MemorySocket:
    def __init__(self, pieces):
        self.pieces, self.sent, self.closed = list(pieces), [], False
    def settimeout(self, seconds):
        assert seconds > 0
    def sendall(self, data):
        self.sent.append(data)
    def recv(self, size):
        if not self.pieces:
            return b''
        data = self.pieces.pop(0)
        if len(data) > size:
            self.pieces.insert(0, data[size:])
        return data[:size]
    def close(self):
        self.closed = True


def reply(headers, body=BODY):
    return b'HTTP/1.1 200 OK\r\n' + headers + b'\r\n' + body


def request(raw, split=None):
    pieces = [raw] if split is None else [raw[i:i+split] for i in range(0, len(raw), split)]
    sock = MemorySocket(pieces)
    client = Ia2Client('127.0.0.1', 1, connect=lambda *_: sock)
    return client.request('POST', '/write', {'name': 'sp', 'value': 1}), sock


def bad_answers():
    cases = {}
    for first, second in ((N, N+10), (N+10, N), (N, N), (N, 2**30)):
        cases['length-%d-%d' % (first, second)] = reply(
            b'Content-Length: %d\r\ncOnTeNt-LeNgTh: %d\r\n' % (first, second))
    for value in (b'%d, %d' % (N, N+10), b'%d, %d' % (N, N), b'', b'-1', b'+1', b'1e2'):
        cases['length-list-or-invalid-%r' % value] = reply(b'Content-Length: ' + value + b'\r\n')
    for first, second in ((b'chunked', b'gzip'), (b'gzip', b'chunked'), (b'chunked', b'chunked')):
        cases['encoding-%r-%r' % (first, second)] = reply(
            b'Transfer-Encoding: ' + first + b'\r\ntRaNsFeR-EnCoDiNg: ' + second + b'\r\n', CHUNKS)
    for encoding in (b'chunked, gzip', b'gzip, chunked', b'chunked, chunked', b''):
        cases['encoding-list-%r' % encoding] = reply(b'Transfer-Encoding: ' + encoding + b'\r\n', CHUNKS)
    for headers in (b'Transfer-Encoding: chunked\r\nContent-Length: %d\r\n' % N,
                    b'Content-Length: %d\r\nTransfer-Encoding: chunked\r\n' % N):
        cases['encoding-and-length-%r' % headers[:10]] = reply(headers, CHUNKS)
    cases['malformed-name-hides-second-length'] = reply(
        b'Content-Length: %d\r\nContent-Length : %d\r\n' % (N, N+10))
    return cases


class Framing(unittest.TestCase):
    def test_all_ambiguous_headers_are_protocol_errors(self):
        for name, raw in bad_answers().items():
            for split in (None, 1, 7):
                with self.subTest(case=name, split=split):
                    rec, sock = request(raw, split)
                    self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'protocol'))
                    self.assertEqual(len(sock.sent), 1)
                    self.assertTrue(sock.closed)

    def test_valid_framing_still_works(self):
        cases = [reply(b'Content-Length: %d\r\n' % N), reply(b'cOnTeNt-LeNgTh: \t%d \t\r\n' % N),
                 reply(b'Transfer-Encoding: ChUnKeD\r\n', CHUNKS), reply(b''),
                 reply(b'Content-Length: %d\r\nX-Trace: a\r\nX-Trace: b\r\n' % N)]
        rng = random.Random(20261028)
        for raw in cases:
            for _ in range(20):
                rec, sock = request(raw, rng.randint(1, 29))
                self.assertEqual(rec['json'], json.loads(BODY))
                self.assertEqual(rec['kind'], 'response')
                self.assertTrue(sock.closed)
                self.assertEqual(len(sock.sent), 1)

    def test_ordinary_truncation_still_fails(self):
        rec, _ = request(reply(b'Content-Length: %d\r\n' % (N+10)))
        self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'truncated'))

    def test_conflict_never_verifies_or_upgrades_after_restart(self):
        for name, raw in bad_answers().items():
            with self.subTest(case=name), tempfile.TemporaryDirectory() as directory:
                clock = [START]
                class TransportClient(Client):
                    def request(self, method, path, body=None, origin=None):
                        self.posts.append({'origin': origin})
                        sock = MemorySocket([raw])
                        return Ia2Client('127.0.0.1', 1, connect=lambda *_: sock).request(method, path, body, origin)
                client = TransportClient(clock)
                reg = reg_for(verify={'feedback': 'sp_fb', 'tolerance': 0, 'settle_ms': 1000})
                e = engine(directory, clock, reg=reg, client=client)
                line = envelope(payload(START))
                try:
                    e.safe_handle_line(line)
                    event = e.journal.final_event(AID)
                    sample = {'event_id': 'fresh-matching', 'native_runtime_id': 'R', 'scan_count': 9,
                              'scan_timestamp_us': event.get('post_scan_us', 4000)+1000,
                              'collected_at_ms': START+1, 'variables': [{'name': 'sp_fb', 'decoded_value': 1,
                                  'input': {'device': 'dev', 'stale': False}}],
                              'device_health': [{'name': 'dev', 'healthy': True}],
                              'watchdog_tripped': False, 'fault': None, 'latch_reason': None, 'forces': []}
                    result = verifier.derive(AID, reg.targets['set'], [event], [sample], START+2, registry_sha256=reg.sha256)
                    self.assertEqual(result['status'], 'unknown')
                    self.assertEqual(event['status'], 'unknown')
                    self.assertEqual(len(client.posts), 1)
                finally:
                    close(e)
                for _ in range(2):
                    e = engine(directory, clock, reg=reg, client=client)
                    try:
                        e.recover(); e.safe_handle_line(line)
                        clock[0] += 6000
                        e.reconcile_unknowns()
                        self.assertEqual(e.journal.final_event(AID), event)
                        self.assertEqual(len(client.posts), 1)
                    finally:
                        close(e)


if __name__ == '__main__':
    unittest.main()
