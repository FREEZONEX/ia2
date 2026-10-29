"""IR02 regression: inject below durable.sync, at fcntl/os.fsync, not above it."""
import errno
import json
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, Client, close, engine, envelope, payload
from bridge import durable
from bridge.journal import JournalWriteError

START = 1800000000000


class Sync(unittest.TestCase):
    def test_available_fullsync_errors_are_never_downgraded(self):
        for code in (errno.EIO, errno.ENOSPC, errno.EBADF, errno.EINVAL, errno.EINTR,
                     errno.ENOTSUP, errno.EOPNOTSUPP):
            with self.subTest(errno=code):
                error = OSError(code, 'injected full sync failure')
                fcntl = types.SimpleNamespace(F_FULLFSYNC=51, fcntl=mock.Mock(side_effect=error))
                with mock.patch.object(durable, 'fcntl', fcntl), mock.patch.object(durable.os, 'fsync') as fallback:
                    with self.assertRaises(OSError) as ctx:
                        durable.sync(123)
                    self.assertIs(ctx.exception, error)
                    fallback.assert_not_called()

    def test_successful_fullsync_does_not_call_fsync(self):
        fcntl = types.SimpleNamespace(F_FULLFSYNC=51, fcntl=mock.Mock())
        with mock.patch.object(durable, 'fcntl', fcntl), mock.patch.object(durable.os, 'fsync') as fallback:
            durable.sync(123)
            fcntl.fcntl.assert_called_once_with(123, 51)
            fallback.assert_not_called()

    def test_fsync_only_when_platform_lacks_fullsync(self):
        for platform in (None, types.SimpleNamespace()):
            with self.subTest(platform=platform):
                with mock.patch.object(durable, 'fcntl', platform), mock.patch.object(durable.os, 'fsync') as fsync:
                    durable.sync(123)
                    fsync.assert_called_once_with(123)
                with mock.patch.object(durable, 'fcntl', platform), mock.patch.object(durable.os, 'fsync', side_effect=OSError(errno.EIO, 'fsync')):
                    with self.assertRaises(OSError):
                        durable.sync(123)


class EngineStorage(unittest.TestCase):
    def test_bottom_layer_failure_stops_each_journal_stage_and_recovers(self):
        for stage in ('intent', 'response', 'post_check', 'terminal'):
            with self.subTest(stage=stage), tempfile.TemporaryDirectory() as directory:
                clock = [START]; client = Client(clock)
                e = engine(directory, clock, client=client)
                line = envelope(payload(START))
                state = {'stage': None}
                original = e.journal.append
                def track(kind, **fields):
                    state['stage'] = kind
                    return original(kind, **fields)
                e.journal.append = track
                fd = e.journal._fh.fileno()
                real = durable.fcntl.fcntl
                def injected(fileno, operation, *args):
                    if fileno == fd and operation == durable.fcntl.F_FULLFSYNC and state['stage'] == stage:
                        raise OSError(errno.EIO, 'IR02 bottom-layer failure at '+stage)
                    return real(fileno, operation, *args)
                try:
                    with mock.patch.object(durable.fcntl, 'fcntl', side_effect=injected):
                        with self.assertRaises(JournalWriteError):
                            e.safe_handle_line(line)
                        self.assertTrue(e.journal.failed)
                        posts, reads = len(client.posts), client.n
                        with self.assertRaises(JournalWriteError):
                            e.safe_handle_line(envelope(payload(START, action_id='b'*32)))
                        self.assertEqual((len(client.posts), client.n), (posts, reads))
                    self.assertEqual(posts, 0 if stage == 'intent' else 1)
                finally:
                    close(e)
                for _ in range(2):
                    e = engine(directory, clock, client=client)
                    try:
                        e.recover(); e.safe_handle_line(line)
                        self.assertEqual(len(client.posts), posts)
                        self.assertIsNotNone(e.journal.final_event(AID))
                    finally:
                        close(e)
                rows = [json.loads(l) for l in Path(directory, 'journal.jsonl').read_text().splitlines()]
                self.assertEqual(sum(r['kind']=='terminal' and r.get('action_id')==AID for r in rows), 1)


if __name__ == '__main__':
    unittest.main()
