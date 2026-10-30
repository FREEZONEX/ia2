"""AR01: raw HTTP header boundaries must agree with parsed fields.

Memory sockets and temporary loopback servers only. Tests exercise the real
client, engine, verifier and bridge process; no result/classifier substitutions.
"""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path
import itertools
import json
from pathlib import Path
import random
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, Client, SRC, close, engine, envelope, payload, reg_for
from test_ir01_framing import BODY, N, CHUNKS, MemorySocket, reply, request
from bridge.ia2client import Ia2Client
import signing_fixtures as verifier
import test_p1_f07_process as proc

START = 1800000000000


def bad_answers():
    length = b'Content-Length: %d\r\n' % N
    conflict = b'Content-Length: %d\r\n' % (N + 10)
    result = {
        'CR-before-conflicting-name': reply(length + b'\r' + conflict),
        'CR-instead-of-LF': reply(b'Content-Length: %d\r\r' % N + conflict),
        'CR-in-unrelated-value': reply(length + b'X-Test: value\r\r' + conflict),
        'CR-in-length-value': reply(b'Content-Length: \r%d\r\n' % N),
        'CR-after-field-value': reply(length + b'X-Test: value\r\r\n'),
        'CR-before-blank': reply(length + b'\r\r\n'),
        'CR-in-status': b'HTTP/1.1 200 OK\rhidden\r\n' + length + b'\r\n' + BODY,
        'CR-after-status': b'HTTP/1.1 200 OK\r\r\n' + length + b'\r\n' + BODY,
        'CR-hides-second-encoding': reply(b'Transfer-Encoding: chunked\r\n\rTransfer-Encoding: gzip\r\n', CHUNKS),
        'CR-hides-length-after-encoding': reply(b'Transfer-Encoding: chunked\r\n\r' + length, CHUNKS),
        'CR-hides-encoding-after-length': reply(length + b'\rTransfer-Encoding: chunked\r\n'),
        'CR-in-interim-head': b'HTTP/1.1 100 Continue\r\nX-Test: value\rhidden\r\n\r\n' + reply(length),
    }
    return result


class Headers(unittest.TestCase):
    def test_bare_CR_is_protocol_error_for_every_segment_size(self):
        for name, raw in bad_answers().items():
            for split in (None, 1, 7):
                with self.subTest(case=name, split=split):
                    rec, sock = request(raw, split)
                    self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'protocol'))
                    self.assertEqual(len(sock.sent), 1)
                    self.assertTrue(sock.closed)

    def test_mailbox_separator_must_not_disappear(self):
        length = b'Content-Length: %d\r\n' % N
        for headers in (b'From somebody\r\n' + length,
                        length + b'From somebody\r\n',
                        b'X-Test: a\r\nFrom somebody\r\n' + length):
            with self.subTest(headers=headers):
                rec, _ = request(reply(headers), 1)
                self.assertEqual((rec['kind'], rec.get('error_kind')), ('error', 'protocol'))

    def test_CRLF_LF_and_mixed_lines_have_identical_boundaries(self):
        rng = random.Random(20261028)
        # Each of the status, two fields and final empty line has its own ending.
        for endings in itertools.product((b'\r\n', b'\n'), repeat=4):
            lines = (b'HTTP/1.1 200 OK', b'Content-Length: %d' % N, b'X-Test: value', b'')
            raw = b''.join(line + end for line, end in zip(lines, endings)) + BODY
            for split in (1, 7, rng.randint(8, 40)):
                with self.subTest(endings=endings, split=split):
                    rec, sock = request(raw, split)
                    self.assertEqual(rec['kind'], 'response')
                    self.assertEqual(rec['text'].encode(), BODY)
                    self.assertEqual(len(sock.sent), 1)
                    self.assertTrue(sock.closed)
        # Preserve duplicate non-framing fields, HTTP folding, chunked and close-delimited bodies.
        cases = [reply(b'X-Test: a\r\nX-Test: b\r\n'),
                 reply(b'X-Test: a\r\n\tb\r\nContent-Length: %d\r\n' % N),
                 reply(b'Transfer-Encoding: chunked\r\n', CHUNKS),
                 b'HTTP/1.1 200 OK\n\r\n' + BODY]
        for raw in cases:
            rec, _ = request(raw, 1)
            self.assertEqual(rec['kind'], 'response')
            self.assertEqual((rec['kind'], rec['text'].encode()), ('response', BODY))

    def test_interim_and_bodyless_heads_still_validate_syntax(self):
        for status in (204, 304):
            for bad in (False, True):
                head = b'HTTP/1.1 %d OK\r\nX-Test: a' % status + (b'\rb' if bad else b'') + b'\r\n\r\n'
                with self.subTest(status=status, bad=bad):
                    rec, _ = request(head, 1)
                    self.assertEqual(rec.get('error_kind'), 'protocol' if bad else None)
                    self.assertEqual(rec['kind'], 'error' if bad else 'response')
        rec, _ = request(b'HTTP/1.1 100 Continue\n\r\n' + reply(b'Content-Length: %d\r\n' % N), 1)
        self.assertEqual(rec['kind'], 'response')
        self.assertEqual(rec['json'], json.loads(BODY))

    def test_head_validation_never_reads_body_as_headers(self):
        body = b'\rbody\r\n\nContent-Length: 999\r\rnot-a-header'
        variants = [reply(b'Content-Length: %d\r\n' % len(body), body),
                    reply(b'', body),
                    reply(b'Transfer-Encoding: chunked\r\n', b'%x\r\n' % len(body) + body + b'\r\n0\r\n\r\n')]
        for raw in variants:
            for boundary in range(1, len(raw)):
                with self.subTest(boundary=boundary):
                    sock = MemorySocket([raw[:boundary], raw[boundary:]])
                    rec = Ia2Client('127.0.0.1', 1, connect=lambda *_: sock).request('POST', '/write')
                    self.assertEqual(rec['kind'], 'response')
                    self.assertEqual((rec['kind'], rec['text'].encode()), ('response', body))

    def test_bad_heads_never_verify_or_upgrade_after_two_restarts(self):
        for name, raw in bad_answers().items():
            with self.subTest(case=name), tempfile.TemporaryDirectory() as directory:
                clock = [START]
                class Wire(Client):
                    def request(self, method, path, body=None, origin=None):
                        self.posts.append({'origin': origin})
                        sock = MemorySocket([raw])
                        return Ia2Client('127.0.0.1', 1, connect=lambda *_: sock).request(method, path, body, origin)
                client = Wire(clock)
                registry = reg_for(verify={'feedback': 'sp_fb', 'tolerance': 0, 'settle_ms': 1000})
                line = envelope(payload(START))
                eng = engine(directory, clock, reg=registry, client=client)
                try:
                    eng.safe_handle_line(line)
                    event = eng.journal.final_event(AID)
                    sample = {'event_id': 'matching-feedback', 'native_runtime_id': 'R', 'scan_count': 9,
                              'scan_timestamp_us': event.get('post_scan_us', 4000) + 1000, 'collected_at_ms': START + 1,
                              'variables': [{'name': 'sp_fb', 'decoded_value': 1, 'input': {'device': 'dev', 'stale': False}}],
                              'device_health': [{'name': 'dev', 'healthy': True}], 'watchdog_tripped': False,
                              'fault': None, 'latch_reason': None, 'forces': []}
                    result = verifier.derive(AID, registry.targets['set'], [event], [sample], START + 2,
                                             registry_sha256=registry.sha256)
                    self.assertEqual((event['status'], result['status']), ('unknown', 'unknown'))
                    self.assertEqual(len(client.posts), 1)
                finally:
                    close(eng)
                for _ in range(2):
                    eng = engine(directory, clock, reg=registry, client=client)
                    try:
                        eng.recover(); eng.safe_handle_line(line)
                        clock[0] += 6000
                        eng.reconcile_unknowns()
                        self.assertEqual(eng.journal.final_event(AID), event)
                        self.assertEqual(len(client.posts), 1)
                    finally:
                        close(eng)


class RawHandler(proc.Handler):
    def reply(self, body):
        raw = json.dumps(body).encode()
        mode = self.server.state['mode']
        bad = (mode == 'pre' and self.path == '/status') or (mode == 'post' and self.command == 'POST')
        fields = b'Content-Length: %d\r\n' % len(raw)
        if bad:
            fields += b'\rContent-Length: %d\r\n' % (len(raw) + 10)
        self.wfile.write(reply(fields, raw)); self.wfile.flush()
        self.close_connection = True


class Process(unittest.TestCase):
    spawn = proc.Process.spawn
    stderr = proc.Process.stderr
    submit = proc.Process.submit
    ended = proc.Process.ended
    posts_of = proc.Process.posts_of
    tearDown = proc.Process.tearDown

    def setUp(self):
        proc.OUT = SRC / 'evidence' / 'process-ar01'
        with mock.patch.object(proc, 'Handler', RawHandler):
            proc.Process.setUp(self)
        self.server.state['mode'] = 'good'

    def test_bad_precheck_sends_zero_POST(self):
        bridge, ready = self.spawn('initial')
        self.assertIn(b'"ready": true', ready)
        self.server.state['mode'] = 'pre'
        self.submit(AID)
        self.assertTrue(proc.wait(lambda: self.ended(AID)))
        self.assertEqual(self.ended(AID)[0]['event']['status'], 'rejected')
        self.assertEqual(self.posts_of(AID), [])

    def test_bad_POST_remains_unknown_through_two_process_restarts(self):
        bridge, ready = self.spawn('initial')
        self.assertIn(b'"ready": true', ready)
        self.server.state['mode'] = 'post'
        self.submit(AID)
        self.assertTrue(proc.wait(lambda: self.ended(AID)))
        event = self.ended(AID)[0]['event']
        self.assertEqual(event['status'], 'unknown')
        for n in range(2):
            bridge.kill(); bridge.wait(timeout=4)
            self.server.state['mode'] = 'good'
            bridge, ready = self.spawn('restart-%d' % n)
            self.assertIn(b'"ready": true', ready)
            self.submit('b' * 32 if n == 0 else 'c' * 32)
            next_id = 'b' * 32 if n == 0 else 'c' * 32
            self.assertTrue(proc.wait(lambda: self.ended(next_id)))
            self.assertEqual(self.ended(next_id)[0]['event']['status'], 'accepted')
            self.assertEqual(self.ended(AID)[0]['event'], event)
            self.assertEqual(len(self.ended(AID)), 1)
            self.assertEqual(len(self.posts_of(AID)), 1)
        (self.dest / 'checked.json').write_text(json.dumps({'status': event['status'], 'posts': len(self.posts_of(AID)),
                                                         'process_restarts': 2}, indent=2) + '\n')


if __name__ == '__main__':
    unittest.main(verbosity=2)
