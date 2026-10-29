"""Current-runtime nullable failsafe reports must not weaken independent safety evidence."""
import copy
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_p0_f04 as f04
from p0_support import AID, close, engine, envelope, payload, reg_for
from bridge import gates, preflight
from bridge.errors import Reject

START = 1800000000000
ROUTES = ('/status', '/health')
PHASES = ('pre', 'post', 'recovery')
NULLS = [(r, 'failsafe_status', None) for r in ROUTES]


class NullableFailsafe(unittest.TestCase):
    def exercise(self, phase, edits, expected='accepted'):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = f04.EvidenceClient(clock, edits, after=phase != 'pre')
            line = envelope(payload(START))
            e = engine(tmp, clock, client=client)
            try:
                if phase == 'recovery':
                    class Crash(Exception): pass
                    def stop(): raise Crash()
                    e.hooks = type('Hooks', (), {'after_response': staticmethod(stop)})()
                    with self.assertRaises(Crash): e.handle_line(line)
                    close(e); e = engine(tmp, clock, client=client); e.recover()
                else:
                    e.safe_handle_line(line)
                saved = e.journal.final_event(AID)
                self.assertEqual(saved['status'], expected)
                self.assertEqual(e.counters['internal_errors'], 0)
                posts = 0 if phase == 'pre' and expected == 'rejected' else 1
                self.assertEqual(len(client.posts), posts)
                for restart in (False, True):
                    if restart:
                        close(e); e = engine(tmp, clock, client=client)
                    e.recover(); e.safe_handle_line(line)
                    self.assertEqual(e.journal.final_event(AID), saved)
                    self.assertEqual(len(client.posts), posts)
                return saved
            finally:
                close(e)

    def test_startup_accepts_absent_null_and_empty_reports(self):
        for status in (f04.MISSING, None, f04.CLEAR):
            for health in (f04.MISSING, None, f04.CLEAR):
                with self.subTest(status=status, health=health):
                    edits = [('/status', 'failsafe_status', status), ('/health', 'failsafe_status', health)]
                    client = f04.EvidenceClient([START], edits)
                    result = preflight.check(client, reg_for())
                    self.assertTrue(result['reachable'])
                    self.assertEqual(result['problems'], [])
                    self.assertEqual(client.posts, [])

    def test_nullable_reports_preserve_acceptance_recovery_and_no_resend(self):
        for phase in PHASES:
            for peer in (f04.MISSING, None, f04.CLEAR):
                for route in ROUTES:
                    with self.subTest(phase=phase, route=route, peer=peer):
                        edits = [(r, 'failsafe_status', None if r == route else peer) for r in ROUTES]
                        self.exercise(phase, edits)

    def test_null_does_not_mask_pending_acknowledged_or_unconfirmed(self):
        for phase in PHASES:
            for route in ROUTES:
                for field in ('pending', 'acknowledged', 'unconfirmed'):
                    with self.subTest(phase=phase, route=route, field=field):
                        report = copy.deepcopy(f04.CLEAR)
                        report[field] = [{'device': 'dev', 'error': 'fixture failure'}] if field == 'unconfirmed' else ['dev']
                        event = self.exercise(phase, NULLS + [(route, 'failsafe_status', report)],
                                              'rejected' if phase == 'pre' else 'unknown')
                        self.assertEqual(event['reason'], 'latched' if phase == 'pre' else 'latched_after_write')

    def test_null_does_not_override_fault_latch_or_watchdog(self):
        for phase in PHASES:
            for route in ROUTES:
                for field, value in (('fault', 'fixture fault'), ('latch_reason', 'fixture latch'), ('watchdog_tripped', True)):
                    with self.subTest(phase=phase, route=route, field=field):
                        self.exercise(phase, NULLS + [(route, field, value)], 'rejected' if phase == 'pre' else 'unknown')

    def test_malformed_report_is_rejected_even_when_peer_is_null(self):
        for route in ROUTES:
            for value in (False, 0, '', 'null', [], {}, {'pending': []}, dict(f04.CLEAR, pending=None), dict(f04.CLEAR, extra=[])):
                edits = NULLS + [(route, 'failsafe_status', value)]
                with self.subTest(route=route, value=value):
                    self.assertTrue(preflight.check(f04.EvidenceClient([START], edits), reg_for())['problems'])
                    for phase in PHASES:
                        event = self.exercise(phase, edits, 'rejected' if phase == 'pre' else 'unknown')
                        self.assertEqual(event['reason'], 'runtime_unreachable')

    def test_null_does_not_mask_missing_required_fields(self):
        for phase in PHASES:
            fields = f04.PRE_FIELDS if phase == 'pre' else f04.POST_FIELDS
            for route, names in fields.items():
                for name in names:
                    with self.subTest(phase=phase, route=route, field=name):
                        event = self.exercise(phase, NULLS + [(route, name, f04.MISSING)],
                                              'rejected' if phase == 'pre' else 'unknown')
                        self.assertEqual(event['reason'], 'runtime_unreachable')

    def test_all_three_prewrite_snapshots_remain_authoritative(self):
        client = f04.EvidenceClient([START], NULLS)
        status_a, _ = client.get_json('/status'); health, _ = client.get_json('/health'); status_b, _ = client.get_json('/status')
        for active in range(3):
            with self.subTest(active=active):
                docs = copy.deepcopy([health, status_a, status_b])
                docs[active]['failsafe_status'] = dict(f04.CLEAR, pending=['dev'])
                with self.assertRaises(Reject) as ctx: gates.check_runtime(*docs)
                self.assertEqual(ctx.exception.reason, 'latched')


if __name__ == '__main__':
    unittest.main(verbosity=2)
