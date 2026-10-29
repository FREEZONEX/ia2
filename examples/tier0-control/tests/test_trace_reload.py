"""A trace holding complete lines the bridge could not have written (non-ASCII bytes, JSON that is not an object, an event ID that is not
a string, nesting too deep to parse): startup skips each one and reports its line number, the same as a line that is not JSON at all,
instead of ending with an uncaught exception."""
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import SRC
from bridge.trace import TraceWriter
import test_p1_f07_process as proc


def line(rec):
    return (json.dumps(rec, sort_keys=True, separators=(',', ':')) + '\n').encode('ascii')


A = line({'event_id': 'a' * 32, 'journal_seq': 2, 'journal_hash': 'h2'})
B = line({'event_id': 'b' * 32, 'journal_seq': 5, 'journal_hash': 'h5'})
FOREIGN = [
    b'caf\xc3\xa9 is not ascii\n',
    '{"event_id":"café"}\n'.encode('utf-8'),        # valid JSON, but the bridge only writes ASCII
    b'not json\n',
    b'[1]\n', b'"text"\n', b'5\n', b'null\n',
    b'{"event_id":7}\n', b'{"event_id":["l"]}\n', b'{"kind":"no event id"}\n',
    b'[' * 100000 + b'\n',                               # json.loads raises RecursionError, not ValueError
]


class Reload(unittest.TestCase):
    def open_with(self, data):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        path = Path(tmp.name, 'trace.jsonl')
        path.write_bytes(data)
        writer = TraceWriter(str(path))
        self.addCleanup(writer.close)
        return writer, path

    def test_foreign_lines_are_skipped_and_numbered(self):
        data = A + b''.join(FOREIGN) + B
        writer, path = self.open_with(data)
        self.assertEqual(writer.seen, {'a' * 32, 'b' * 32})
        self.assertEqual(writer.last_anchor, (5, 'h5'))
        self.assertEqual(writer.skipped, list(range(2, 2 + len(FOREIGN))))
        self.assertEqual(path.read_bytes(), data, 'complete lines are kept as they are')
        self.assertTrue(writer.emit({'event_id': 'c' * 32}))
        self.assertEqual(path.read_bytes(), data + line({'event_id': 'c' * 32}))

    def test_an_anchor_needs_a_non_negative_integer_seq_and_a_string_hash(self):
        ids = ['%032d' % n for n in range(6)]
        recs = [{'event_id': ids[0], 'journal_seq': 1, 'journal_hash': 'h1'},
                {'event_id': ids[1], 'journal_seq': True, 'journal_hash': 'h'},
                {'event_id': ids[2], 'journal_seq': -1, 'journal_hash': 'h'},
                {'event_id': ids[3], 'journal_seq': '9', 'journal_hash': 'h'},
                {'event_id': ids[4], 'journal_seq': 9},
                {'event_id': ids[5], 'journal_seq': 9, 'journal_hash': 9}]
        writer, _ = self.open_with(b''.join(line(r) for r in recs))
        self.assertEqual(writer.seen, set(ids), 'the events are still known, so they are not published again')
        self.assertEqual(writer.last_anchor, (1, 'h1'))
        self.assertEqual(writer.skipped, [])

    def test_a_torn_tail_is_still_cut_and_kept_apart(self):
        writer, path = self.open_with(A + b'\xc3\xa9{"event_id":')
        self.assertEqual(path.read_bytes(), A)
        self.assertEqual(Path(writer.torn_copy).read_bytes(), b'\xc3\xa9{"event_id":')
        Path(writer.torn_copy).unlink()
        self.assertEqual(writer.skipped, [])


class Process(unittest.TestCase):
    spawn = proc.Process.spawn
    stderr = proc.Process.stderr
    tearDown = proc.Process.tearDown

    def setUp(self):
        proc.OUT = SRC / 'evidence' / 'process-trace-reload'
        proc.Process.setUp(self)

    def test_the_bridge_starts_and_says_which_trace_lines_it_skipped(self):
        # No anchors here: the fresh journal is empty, and an anchor past its head is refused (exit 6) before anything else.
        (self.dest / 'trace.jsonl').write_bytes(line({'event_id': 'a' * 32}) + FOREIGN[0] + FOREIGN[3] + line({'event_id': 'b' * 32}))
        p, ready = self.spawn('bridge')
        self.assertIn(b'"ready": true', ready, self.stderr('bridge'))
        self.assertIn('skipped 2 trace line(s) that are not bridge records (lines 2, 3)', self.stderr('bridge'))
        self.assertIsNone(p.poll())


if __name__ == '__main__':
    unittest.main(verbosity=2)
