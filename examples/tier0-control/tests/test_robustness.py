"""Poison lines must not end the bridge: odd signed payloads, unexpected exceptions, repeated bad lines across a restart."""
import json
import os
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import robustness_cases as rc  # noqa: E402
from bridge import auth  # noqa: E402
from bridge.engine import Engine  # noqa: E402
from bridge.journal import Journal  # noqa: E402
from bridge.trace import TraceWriter  # noqa: E402


class Robustness(unittest.TestCase):
    def test_no_odd_signed_payload_makes_handle_line_raise(self):
        problems, n = rc.run_all('handle_line', verbose=False)
        self.assertEqual(problems, [], 'inputs that raised: %s' % problems)
        self.assertGreaterEqual(n, 25)

    def test_the_guarded_entry_point_never_raises_either(self):
        problems, _ = rc.run_all('safe_handle_line', verbose=False)
        self.assertEqual(problems, [])

    def engine(self, tmp, client):
        keys = {'k1': {'secret': b'\x07' * 32, 'not_before_ms': 0, 'not_after_ms': 2 ** 60}}
        jr = Journal(os.path.join(tmp, 'j.jsonl'), lambda: 1000)
        jr.open()
        t = [1_700_000_000_000]

        def clock():
            t[0] += 1
            return t[0]
        reg = rc.registry.parse(json.dumps({'version': 1, 'targets': [{
            'key': 'flow', 'device_id': 'bench01-sim', 'ia2_variable': 'sp_flow', 'type': 'REAL', 'min': 0.0, 'max': 40.0,
            'simulation': True, 'enabled': True, 'require_devices': ['coupler'], 'min_interval_ms': 0, 'max_rate_per_min': 60}]}), 'x')
        eng = Engine({'poll_ms': 1, 'advance_wait_ms': 50, 'hold_path': None, 'project_dir': None}, reg, keys, jr, client,
                     TraceWriter(os.path.join(tmp, 't.jsonl')), clock=clock, sleep=lambda s: None)
        return eng, jr, keys, t

    def good_line(self, keys, t):
        p = rc.payload(requested_at_ms=t[0], deadline_ms=t[0] + 5000)
        text = json.dumps(p)
        return json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': text, 'sig': auth.sign(keys['k1']['secret'], text)}), p

    def test_an_exception_mid_send_quarantines_the_line_and_concludes_the_action_without_resending(self):
        class Exploding(rc.FakeClient):
            posts = 0

            def request(self, method, path, body=None, origin=None):
                Exploding.posts += 1
                raise RuntimeError('boom')
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, keys, t = self.engine(tmp, Exploding())
            line, p = self.good_line(keys, t)
            eng.safe_handle_line(line)                                         # must not raise
            kinds = [r['kind'] for r in rc.read_journal(jr.path)]
            self.assertIn('internal_error', kinds)
            ev = jr.final_event(p['action_id'])
            self.assertIsNotNone(ev, 'the action in flight was concluded')
            self.assertEqual(ev['status'], 'unknown')
            self.assertEqual(Exploding.posts, 1)
            jr.close()
            # a restart re-reads the same line: it is skipped, nothing is sent
            eng2, jr2, _, _ = self.engine(tmp, Exploding())
            eng2.safe_handle_line(line)
            self.assertEqual(Exploding.posts, 1, 'the quarantined line was not processed again')
            jr2.close()
            eng.trace.close()
            eng2.trace.close()

    def test_a_line_that_raised_before_any_intent_is_not_retried_after_a_restart(self):
        class Exploding(rc.FakeClient):
            reads = 0

            def get_json(self, path):
                Exploding.reads += 1
                raise RuntimeError('boom while reading the runtime')
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, keys, t = self.engine(tmp, Exploding())
            line, p = self.good_line(keys, t)
            eng.safe_handle_line(line)
            reads = Exploding.reads
            self.assertEqual([r['kind'] for r in rc.read_journal(jr.path)].count('internal_error'), 1)
            self.assertNotIn(p['action_id'], jr.actions, 'nothing was sent, so no action exists')
            jr.close()
            eng2, jr2, _, _ = self.engine(tmp, Exploding())
            eng2.safe_handle_line(line)                                        # after a restart
            self.assertEqual(Exploding.reads, reads, 'the quarantined line was not run again')
            self.assertEqual([r['kind'] for r in rc.read_journal(jr2.path)].count('internal_error'), 1)
            jr2.close()
            eng.trace.close()
            eng2.trace.close()

    def test_a_bad_line_re_read_after_a_restart_is_not_journaled_twice(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, keys, t = self.engine(tmp, rc.FakeClient())
            bad = json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': '{"x":1}', 'sig': '0' * 64})
            nan = json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': '{"target": NaN}',
                              'sig': auth.sign(keys['k1']['secret'], '{"target": NaN}')})
            for line in (bad, nan):
                eng.safe_handle_line(line)
                eng.safe_handle_line(line)                                     # again in the same life
            n1 = len(rc.read_journal(jr.path))
            jr.close()
            eng2, jr2, _, _ = self.engine(tmp, rc.FakeClient())
            for line in (bad, nan):
                eng2.safe_handle_line(line)                                    # again after a restart
            self.assertEqual(len(rc.read_journal(jr2.path)), n1)
            jr2.close()
            eng.trace.close()
            eng2.trace.close()

    def test_journal_and_trace_take_any_string(self):
        with tempfile.TemporaryDirectory() as tmp:
            jr = Journal(os.path.join(tmp, 'j.jsonl'), lambda: 1)
            jr.open()
            jr.append('x', weird='\ud800 \x00 温度 \U0001F600')
            jr.close()
            jr2 = Journal(os.path.join(tmp, 'j.jsonl'), lambda: 1)
            jr2.open()                                                         # the hash chain still verifies
            self.assertEqual(jr2.seq, 1)
            jr2.close()
            tw = TraceWriter(os.path.join(tmp, 't.jsonl'))
            self.assertTrue(tw.emit({'event_id': 'a', 'x': '\udfff 温度'}))
            tw.close()


if __name__ == '__main__':
    unittest.main()
