"""D6 must use TOML semantics in source and production bridge processes."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import SRC
from bridge import northbound_guard
from bridge.errors import Reject
import test_p1_f07_process as proc

ESCAPED = '[mqtt]\nbroker_host = "127.0.0.1"\n"allow_\\u0077rite" = true\n'
INLINE = 'mqtt = { broker_host = "127.0.0.1", password = "fixture#value", allow_write = true }\n'
SAFE = '[mqtt]\nbroker_host = "127.0.0.1"\nallow_write = false\n'


class Guard(unittest.TestCase):
    def check(self, text):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, 'northbound.toml').write_text(text, encoding='utf-8')
            return northbound_guard.check(tmp)

    def denied(self, text):
        with self.assertRaises(Reject) as ctx:
            self.check(text)
        self.assertEqual(ctx.exception.reason, 'northbound_write_enabled')
        self.assertNotIn('fixture#value', ctx.exception.detail)

    def test_all_supported_true_spellings_are_rejected(self):
        cases = [ESCAPED, INLINE,
                 'mqtt.allow_write = true\n',
                 '"mq\\u0074t"."allow_\\U00000077rite" = true\n',
                 "['mqtt']\n'allow_write' = true\n",
                 '["mq\\u0074t"]\n"allow_\\u0077rite" = true\n',
                 '[mqtt]\nenabled = false\nallow_write = true\n',
                 'mqtt = { password = "allow_write = false,", allow_write = true }\n',
                 "mqtt = { password = 'fixture#value', allow_write = true }\n"]
        for text in cases:
            with self.subTest(text=text): self.denied(text)

    def test_false_and_omitted_flags_follow_runtime_defaults(self):
        cases = ['', '# allow_write = true\n', SAFE,
                 '[mqtt]\nbroker_host = "x"\n',
                 ESCAPED.replace('= true', '= false'), INLINE.replace('= true', '= false'),
                 'mqtt.allow_write = false\n',
                 '[mqtt]\npassword = "allow_write = true"\n',
                 '[mqtt]\npassword = """fixture#value\nallow_write = true\n"""\n',
                 '[mqtt]\npassword = \'\'\'fixture#value\nallow_write = true\n\'\'\'\n',
                 '[other]\nallow_write = true\n',
                 '[mqtt.other]\nallow_write = true\n']
        for text in cases:
            with self.subTest(text=text):
                try:
                    actual = self.check(text)
                except Reject as exc:
                    self.fail('safe configuration refused: ' + exc.detail)
                self.assertEqual(actual, hashlib.sha256(text.encode()).hexdigest())

    def test_invalid_toml_and_wrong_types_fail_closed(self):
        cases = ['[mqtt', 'mqtt = false', 'mqtt = []', 'mqtt = "off"', '[[mqtt]]\nallow_write = false\n',
                 '[mqtt]\nallow_write = false\nallow_write = true\n',
                 '[mqtt]\nallow_write = false\n"allow_\\u0077rite" = true\n',
                 '[mqtt]\nallow_write = false\npassword = "fixture#value\n']
        cases += ['[mqtt]\nallow_write = ' + v for v in ('0', '1', '"false"', "'false'", '[]', '{}', 'false_ish')]
        for text in cases:
            with self.subTest(text=text): self.denied(text)


class Process(unittest.TestCase):
    spawn = proc.Process.spawn
    stderr = proc.Process.stderr
    submit = proc.Process.submit
    ended = proc.Process.ended
    tearDown = proc.Process.tearDown

    def setUp(self):
        proc.OUT = SRC / 'evidence' / 'process-d6'
        proc.Process.setUp(self)
        self.source_cmd = list(self.cmd)
        self.package = self.dest / 'prod'
        p = subprocess.run([sys.executable, '-I', '-B', str(SRC / 'tools/package_prod.py'), str(self.package)],
                           capture_output=True, text=True, timeout=10)
        self.assertEqual(p.returncode, 0, p.stderr)

    def flavor(self, name):
        self.cmd = list(self.source_cmd)
        if name == 'prod':
            self.cmd = [str(self.package / 'bridge_run.py') if p == str(SRC / 'bridge_run.py') else p for p in self.cmd]

    def test_startup_refuses_before_runtime_reads_in_both_packages(self):
        for flavor in ('lab', 'prod'):
            self.flavor(flavor)
            for i, text in enumerate((ESCAPED, INLINE, '[mqtt]\nallow_write = "false"', '[mqtt')):
                with self.subTest(flavor=flavor, case=i):
                    Path(self.dest, 'northbound.toml').write_text(text)
                    tag = 'refused-%s-%d' % (flavor, i)
                    p, line = self.spawn(tag)
                    try:
                        self.assertEqual(line, b'')
                        self.assertEqual(p.wait(timeout=5), 3)
                        self.assertIn('startup refused:', self.stderr(tag))
                        self.assertNotIn('fixture#value', self.stderr(tag))
                    finally:
                        if p.poll() is None: p.kill(); p.wait(timeout=5)
        self.assertEqual(self.server.state, dict(posts=[], reads=[]))
        self.assertFalse((self.dest / 'journal.jsonl').exists())

    def test_changed_configuration_blocks_new_requests_and_preserves_replay(self):
        for i, flavor in enumerate(('lab', 'prod')):
            self.flavor(flavor)
            Path(self.dest, 'northbound.toml').write_text(SAFE)
            p, line = self.spawn('live-' + flavor)
            try:
                self.assertIn(b'"ready": true', line)
                for j, text in enumerate((ESCAPED, INLINE)):
                    aid = ('%x' % (i * 4 + j * 2 + 1)) * 32
                    Path(self.dest, 'northbound.toml').write_text(text)
                    before_reads = len(self.server.state['reads'])
                    self.submit(aid)
                    self.assertTrue(proc.wait(lambda: self.ended(aid)))
                    event = self.ended(aid)[0]['event']
                    self.assertEqual((event['status'], event['reason']), ('rejected', 'northbound_write_enabled'))
                    self.assertEqual(len(self.server.state['reads']), before_reads)
                    self.assertEqual(self.server.state['posts'], [])
                    self.submit(aid)
                    # A later request is a FIFO barrier: the replay was handled.
                    barrier = ('%x' % (i * 4 + j * 2 + 2)) * 32
                    self.submit(barrier)
                    self.assertTrue(proc.wait(lambda: self.ended(barrier)))
                    self.assertEqual(len(self.ended(aid)), 1)
                    self.assertEqual(self.server.state['posts'], [])
            finally:
                p.terminate(); p.wait(timeout=5)

    def test_production_contains_unmodified_parser_and_license(self):
        source = SRC / 'bridge/_vendor/tomli'
        provenance = json.loads((source / 'SOURCE.json').read_text())
        for name, expected in provenance['files_sha256'].items():
            with self.subTest(file=name):
                copied = self.package / 'bridge/_vendor/tomli' / name
                self.assertTrue(copied.exists())
                self.assertEqual(hashlib.sha256(copied.read_bytes()).hexdigest(), expected)


if __name__ == '__main__':
    unittest.main(verbosity=2)
