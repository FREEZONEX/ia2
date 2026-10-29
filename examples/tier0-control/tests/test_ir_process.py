"""Real bridge process regressions for IR01/IR02, ephemeral loopback servers only."""
import json
import os
import sys
import time
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_p1_f07_process as fixture
from p0_support import AID, SRC, envelope, payload

fixture.OUT = SRC/'evidence'/'process-ir'
HOOKS = str(Path(__file__).resolve().parent/'hooks_ir02_sync.py')


class FramingHandler(fixture.Handler):
    def reply(self, body):
        raw = json.dumps(body).encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(raw)))
        mode = self.server.state.get('mode')
        if (mode == 'bad-post' and self.command == 'POST') or (mode == 'bad-pre' and self.path == '/status'):
            self.send_header('cOnTeNt-LeNgTh', str(len(raw)+10))
        self.end_headers()
        self.wfile.write(raw)


class Process(unittest.TestCase):
    tearDown = fixture.Process.tearDown
    spawn = fixture.Process.spawn
    stderr = fixture.Process.stderr
    submit = fixture.Process.submit
    ended = fixture.Process.ended
    posts_of = fixture.Process.posts_of

    def setUp(self):
        with mock.patch.object(fixture, 'Handler', FramingHandler):
            fixture.Process.setUp(self)

    def stop(self, p):
        if p.poll() is None:
            p.kill(); p.wait(timeout=4)

    def test_ir01_bad_precheck_is_zero_POST(self):
        p, ready = self.spawn('first')
        self.assertIn(b'"ready": true', ready)
        self.server.state['mode'] = 'bad-pre'
        self.submit(AID)
        self.assertTrue(fixture.wait(lambda: self.ended(AID)))
        self.assertEqual(self.ended(AID)[0]['event']['status'], 'rejected')
        self.assertEqual(self.posts_of(AID), [])

    def test_ir01_bad_post_remains_unknown_after_restart(self):
        p, ready = self.spawn('first')
        self.assertIn(b'"ready": true', ready)
        self.server.state['mode'] = 'bad-post'
        self.submit(AID)
        self.assertTrue(fixture.wait(lambda: self.ended(AID)))
        event = self.ended(AID)[0]['event']
        self.assertEqual(event['status'], 'unknown')
        self.assertEqual(len(self.posts_of(AID)), 1)
        self.stop(p)
        self.server.state['mode'] = 'good'
        p, ready = self.spawn('restart')
        self.assertIn(b'"ready": true', ready)
        self.submit('b'*32)
        self.assertTrue(fixture.wait(lambda: self.ended('b'*32)))
        self.assertEqual(self.ended(AID)[0]['event'], event)
        self.assertEqual(len(self.ended(AID)), 1)
        self.assertEqual(len(self.posts_of(AID)), 1)
        self.assertEqual(self.ended('b'*32)[0]['event']['status'], 'accepted')

    def sync_case(self, stage, layer='full'):
        with mock.patch.dict(os.environ, {'IR02_FAIL_AT': stage, 'IR02_SYNC_CALL': layer}):
            p, ready = self.spawn('first', ['--test-hooks', HOOKS])
        self.assertIn(b'"ready": true', ready)
        self.submit(AID)
        self.assertTrue(fixture.wait(lambda: p.poll() is not None or self.ended(AID)))
        # A terminal can have reached disk just before its sync failed; wait for exit.
        self.assertTrue(fixture.wait(lambda: p.poll() is not None, timeout=2), 'sync failure did not stop the bridge')
        self.assertEqual(p.returncode, 7)
        self.assertIn('IR02 injected', self.stderr('first'))
        posts = 0 if stage == 'intent' else 1
        self.assertEqual(len(self.posts_of(AID)), posts)
        self.submit('b'*32)
        for n in range(2):
            p, ready = self.spawn('restart-%d' % n)
            self.assertIn(b'"ready": true', ready)
            self.assertTrue(fixture.wait(lambda: self.ended('b'*32)))
            self.assertEqual(self.ended('b'*32)[0]['event']['status'], 'accepted')
            self.assertEqual(len(self.posts_of(AID)), posts)
            self.assertEqual(len(self.posts_of('b'*32)), 1)
            self.assertEqual(len(self.ended(AID)), 1)
            self.stop(p)
        (self.dest/'checked.json').write_text(json.dumps(dict(stage=stage, layer=layer, exit_code=7,
            first_action_posts=posts, next_action_posts=1, first_action_terminal=self.ended(AID)[0]['event']['status']), indent=2)+'\n')

    def test_ir02_fullsync_intent(self): self.sync_case('intent')
    def test_ir02_fullsync_response(self): self.sync_case('response')
    def test_ir02_fullsync_post_check(self): self.sync_case('post_check')
    def test_ir02_fullsync_terminal(self): self.sync_case('terminal')
    def test_ir02_plain_fsync_intent(self): self.sync_case('intent', 'plain')


if __name__ == '__main__':
    unittest.main()
