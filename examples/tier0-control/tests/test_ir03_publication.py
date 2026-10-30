"""IR03: recover durable results through real request, startup and periodic paths."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path, decode_event, event_record
import errno
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, Client, SRC, close, engine, envelope, payload
from bridge import durable
from bridge.engine import sha256_text
from bridge.journal import JournalWriteError
from signing_fixtures import TraceWriter
import test_p1_f07_process as proc

START = 1800000000000
from legacy_ir03 import LegacyEngine
LEGACY_RUNNER = Path(__file__).with_name('legacy_ir03.py')
HOOK = str(Path(__file__).with_name('hooks_ir03_trace.py'))


class FileFault:
    def __init__(self, fh, mode):
        self.fh, self.mode, self.once = fh, mode, True
    def __getattr__(self, key):
        return getattr(self.fh, key)
    def write(self, data):
        if self.mode == 'short':
            return self.fh.write(data[:3])
        if self.once:
            self.once = False
            if self.mode == 'partial':
                self.fh.write(data[:17])
                self.fh.flush()
            raise OSError(errno.ENOSPC, 'IR03 injected write failure')
        return self.fh.write(data)


def events(path):
    if not Path(path).exists():
        return []
    raw = Path(path).read_bytes()
    if raw and not raw.endswith(b'\n'):
        raise AssertionError('trace contains an incomplete final line')
    try:
        return [decode_event(json.loads(line)) for line in raw.splitlines()]
    except ValueError as exc:
        raise AssertionError('trace contains a corrupt JSON line: %s' % exc)


def semantic(event):
    return {k: v for k, v in event.items() if k not in ('journal_seq', 'journal_hash')}


class Publication(unittest.TestCase):
    def retry(self, e, **kw):
        fn = getattr(e, 'retry_terminal_events', None)
        self.assertTrue(callable(fn), 'durable publication needs a periodic retry entry point')
        fn(**kw)

    def check(self, e, directory, saved, client, expected_posts=1):
        mine = [r for r in events(Path(directory) / 'trace.jsonl') if r.get('action_id') == AID]
        self.assertEqual(len(mine), 1)
        self.assertEqual(semantic(mine[0]), saved)
        self.assertEqual(e.journal.final_event(AID), saved)
        self.assertEqual(len(client.posts), expected_posts)
        self.assertIsNone(e.journal.check_anchor((mine[0]['journal_seq'], mine[0]['journal_hash'])))

    def test_safe_replay_publishes_without_quarantining_or_reclassifying(self):
        for mode in ('accepted', 'unknown', 'rejected'):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp:
                clock = [START]; client = Client(clock); client.bad_after_post = mode == 'unknown'
                e = engine(tmp, clock, client=client)
                line = envelope(payload(START, target=999 if mode == 'rejected' else 1))
                try:
                    with mock.patch.object(e.trace, 'emit', side_effect=OSError(errno.ENOSPC, 'no space')):
                        e.safe_handle_line(line)
                    saved = e.journal.final_event(AID)
                    self.assertEqual(saved['status'], mode)
                    self.assertNotIn(sha256_text(line), e.journal.quarantined)
                    seq, reads = e.journal.seq, client.n
                    for _ in range(3): e.safe_handle_line(line)
                    self.assertEqual((e.journal.seq, client.n), (seq, reads))
                    self.check(e, tmp, saved, client, 0 if mode == 'rejected' else 1)
                finally: close(e)

    def test_periodic_retry_recovers_without_request_replay(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]; client = Client(clock); e = engine(tmp, clock, client=client)
            try:
                with mock.patch.object(e.trace, 'emit', side_effect=OSError(errno.EIO, 'offline')):
                    e.safe_handle_line(envelope(payload(START)))
                    for _ in range(3): self.retry(e)
                saved = e.journal.final_event(AID); seq, reads = e.journal.seq, client.n
                self.assertEqual(e.status_snapshot().get('pending_trace_results'), 1)
                self.retry(e); self.retry(e)
                self.assertEqual(e.status_snapshot()['pending_trace_results'], 0)
                self.assertEqual((e.journal.seq, client.n), (seq, reads))
                self.check(e, tmp, saved, client)
            finally: close(e)

    def test_old_quarantined_result_older_than_horizon_is_published_at_startup(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]; client = Client(clock); e = engine(tmp, clock, client=client)
            legacy = LegacyEngine(e.cfg, e.registry, e.keys, e.journal, client, e.trace, clock=lambda: clock[0], sleep=lambda _: None)
            line = envelope(payload(START))
            with mock.patch.object(e.trace, 'emit', side_effect=OSError(errno.ENOSPC, 'old trace failure')):
                legacy.safe_handle_line(line)
            saved = e.journal.final_event(AID)
            self.assertIn(sha256_text(line), e.journal.quarantined)
            close(e)
            clock[0] += 3 * 86400000
            for _ in range(2):
                e = engine(tmp, clock, client=client)
                try:
                    self.assertNotIn(AID, e.journal.actions)
                    seq = e.journal.seq
                    with mock.patch.object(client, 'get_json', side_effect=AssertionError('publication must not read runtime')):
                        e.recover(); e.safe_handle_line(line)
                    self.assertEqual(e.journal.seq, seq)
                    self.assertIn(sha256_text(line), e.journal.quarantined)
                    self.check(e, tmp, saved, client)
                finally: close(e)

    def test_request_processing_errors_still_quarantine(self):
        with tempfile.TemporaryDirectory() as tmp:
            e = engine(tmp, [START]); line = envelope(payload(START))
            try:
                with mock.patch.object(e.client, 'get_json', side_effect=RuntimeError('request failure')) as calls:
                    e.safe_handle_line(line); n = calls.call_count
                    e.safe_handle_line(line)
                    self.assertEqual(calls.call_count, n)
                self.assertIn(sha256_text(line), e.journal.quarantined)
                self.assertEqual(e.client.posts, [])
            finally: close(e)

    def test_failed_journal_cannot_publish_pending_results(self):
        with tempfile.TemporaryDirectory() as tmp:
            e = engine(tmp, [START])
            try:
                with mock.patch.object(e.trace, 'emit', side_effect=OSError(errno.EIO, 'trace')):
                    e.safe_handle_line(envelope(payload(START)))
                with mock.patch.object(durable, 'sync', side_effect=OSError(errno.EIO, 'journal')):
                    with self.assertRaises(JournalWriteError): e.journal.append('start')
                with mock.patch.object(e.trace, 'emit') as emit:
                    with self.assertRaises(JournalWriteError): self.retry(e)
                    emit.assert_not_called()
            finally: close(e)

    def test_bounded_retry_rotates_failures_and_publishes_other_results(self):
        with tempfile.TemporaryDirectory() as tmp:
            e = engine(tmp, [START]); ids = [str(i) * 32 for i in range(1, 5)]
            try:
                with mock.patch.object(e.trace, 'emit', side_effect=OSError(errno.EIO, 'trace')):
                    for aid in ids: e.safe_handle_line(envelope(payload(START, action_id=aid)))
                original = e.trace.emit; attempts = []
                def emit(event, anchor=None):
                    attempts.append(event['action_id'])
                    if event['action_id'] == ids[0]: raise OSError(errno.EIO, 'one result')
                    return original(event, anchor)
                with mock.patch.object(e.trace, 'emit', side_effect=emit):
                    self.retry(e, limit=2); self.assertEqual(len(attempts), 2)
                    self.retry(e, limit=2); self.assertEqual(len(attempts), 4)
                self.assertEqual(e.status_snapshot().get('pending_trace_results'), 1)
                self.retry(e)
                self.assertEqual({r['action_id'] for r in events(Path(tmp) / 'trace.jsonl')}, set(ids))
                self.assertEqual(len(e.client.posts), 4)
            finally: close(e)

    def test_partial_write_retry_preserves_prior_and_later_events(self):
        with tempfile.TemporaryDirectory() as tmp:
            e = engine(tmp, [START]); client = e.client
            try:
                e.trace.emit({'event_id': 'prior', 'kind': 'bridge_notice'})
                original = e.trace._fh; e.trace._fh = FileFault(original, 'partial')
                e.safe_handle_line(envelope(payload(START))); saved = e.journal.final_event(AID)
                e.trace._fh = original
                e.trace.emit({'event_id': 'later', 'kind': 'bridge_notice'})
                self.retry(e); self.retry(e)
                self.check(e, tmp, saved, client)
                self.assertEqual([r['event_id'] for r in events(Path(tmp) / 'trace.jsonl')], ['prior', 'later', saved['event_id']])
            finally: close(e)

    def test_complete_write_sync_failure_retries_without_duplicate(self):
        for restart in (False, True):
            with self.subTest(restart=restart), tempfile.TemporaryDirectory() as tmp:
                clock = [START]; e = engine(tmp, clock); client = e.client
                fd, real = e.trace._fh.fileno(), durable.sync
                def fail_trace(fileno):
                    if fileno == fd: raise OSError(errno.EIO, 'trace sync')
                    return real(fileno)
                with mock.patch.object(durable, 'sync', side_effect=fail_trace):
                    e.safe_handle_line(envelope(payload(START)))
                saved = e.journal.final_event(AID)
                if restart:
                    close(e); e = engine(tmp, clock, client=client)
                try:
                    e.recover(); self.retry(e)
                    self.check(e, tmp, saved, client)
                finally: close(e)

    def test_short_writes_are_completed_before_marking_seen(self):
        with tempfile.TemporaryDirectory() as tmp:
            writer = TraceWriter(str(Path(tmp) / 'trace.jsonl'))
            try:
                writer._fh = FileFault(writer._fh, 'short')
                writer.emit({'event_id': 'short', 'value': 'abcdef'})
                self.assertEqual(events(writer.path), [event_record({'event_id': 'short', 'value': 'abcdef'})])
            finally: writer.close()

    def test_restart_preserves_torn_tail_and_republishes_complete_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]; e = engine(tmp, clock); client = e.client
            e.trace._fh = FileFault(e.trace._fh, 'partial')
            e.safe_handle_line(envelope(payload(START))); saved = e.journal.final_event(AID)
            close(e); torn = Path(tmp, 'trace.jsonl').read_bytes()
            e = engine(tmp, clock, client=client)
            try:
                backup = getattr(e.trace, 'torn_copy', None)
                self.assertIsNotNone(backup)
                self.assertEqual(Path(backup).read_bytes(), torn)
                e.recover(); self.retry(e)
                self.check(e, tmp, saved, client)
            finally: close(e)

    def test_existing_complete_trace_must_sync_before_suppressing_republication(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp) / 'trace.jsonl')
            Path(path).write_text('{"event_id":"old"}\n')
            with mock.patch.object(durable, 'sync', side_effect=OSError(errno.EIO, 'startup sync')):
                with self.assertRaises(OSError): TraceWriter(path)
            self.assertEqual(Path(path).read_text(), '{"event_id":"old"}\n')


class Process(unittest.TestCase):
    spawn = proc.Process.spawn
    stderr = proc.Process.stderr
    submit = proc.Process.submit
    ended = proc.Process.ended
    posts_of = proc.Process.posts_of
    tearDown = proc.Process.tearDown

    def setUp(self):
        proc.OUT = SRC / 'evidence' / 'process-ir03'
        proc.Process.setUp(self)
        self.marker = self.dest / 'trace-blocked'
        self.marker.write_text('blocked')

    def start_blocked(self, old=False):
        original = list(self.cmd)
        if old:
            self.cmd = [str(LEGACY_RUNNER) if s == str(SRC / 'bridge_run.py') else s for s in self.cmd]
            self.cmd.insert(1, '-B')
        try:
            with mock.patch.dict(os.environ, {'IR03_TRACE_BLOCK_FILE': str(self.marker)}):
                p, ready = self.spawn('blocked', ['--test-hooks', HOOK])
        finally: self.cmd = original
        self.assertIn(b'"ready": true', ready); self.submit(AID)
        self.assertTrue(proc.wait(lambda: self.ended(AID)))
        if old:
            self.assertTrue(proc.wait(lambda: any(r['kind'] == 'internal_error' for r in proc.rows(self.dest / 'journal.jsonl'))))
        else:
            def pending():
                try: return json.loads((self.dest / 'journal.jsonl.status.json').read_text()).get('pending_trace_results') == 1
                except (OSError, ValueError): return False
            self.assertTrue(proc.wait(pending))
        self.assertEqual([r for r in proc.rows(self.dest / 'trace.jsonl') if r.get('action_id') == AID], [])
        return p, self.ended(AID)[0]['event']

    def observed(self, saved):
        mine = [r for r in events(self.dest / 'trace.jsonl') if r.get('action_id') == AID]
        self.assertEqual(len(mine), 1)
        self.assertEqual(semantic(mine[0]), saved)
        self.assertEqual(len(self.posts_of(AID)), 1)
        self.assertEqual(len(self.ended(AID)), 1)
        (self.dest / 'checked.json').write_text(json.dumps({'terminal': saved['status'], 'trace_events': len(mine),
                                                         'posts': len(self.posts_of(AID))}, indent=2) + '\n')

    def test_periodic_retry_without_request_replay(self):
        p, saved = self.start_blocked()
        self.marker.unlink()
        self.assertTrue(proc.wait(lambda: any(r.get('action_id') == AID for r in proc.rows(self.dest / 'trace.jsonl'))))
        self.assertIsNone(p.poll()); self.observed(saved)

    def restart_case(self, old=False):
        p, saved = self.start_blocked(old)
        p.kill(); p.wait(timeout=4)
        self.marker.unlink(); (self.dest / 'requests.jsonl').write_text('')
        for n in range(2):
            p, ready = self.spawn('restart-%d' % n)
            self.assertIn(b'"ready": true', ready)
            self.observed(saved)
            p.kill(); p.wait(timeout=4)

    def test_startup_retry_with_empty_request_file(self): self.restart_case()
    def test_upgrade_from_old_quarantined_journal(self): self.restart_case(old=True)


if __name__ == '__main__':
    unittest.main(verbosity=2)
