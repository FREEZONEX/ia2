"""Table-driven vectors for lab/verifier.py (CONTRACT section 7; TEST-CASES E1-E7 in miniature).  The verifier is tested on
synthetic observations, with nothing from the bridge involved.

Clock layout used below (all in the runtime's SCAN clock, microseconds, unless named _ms):
  the bridge's first post-write /status read showed scan time 5_000_000 (post_scan_us) at wall 6_000 ms  -> anchor 1_000 ms
  settle 5_000 ms  -> the proof window is (5_000_000, 10_000_000];  deadline 21_000 ms -> 20_000_000 us"""
import os
import sys
import unittest

sys.dont_write_bytecode = True
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, 'lab'))
import verifier  # noqa: E402

AID = 'a' * 32
REQ = {'action_id': AID}
ENTRY = {'ia2_variable': 'sp_flow', 'type': 'REAL', 'verify': {'feedback': 'sp_flow_fb', 'tolerance': 0.05, 'settle_ms': 5000}}
ACC = {'kind': 'action_status', 'action_id': AID, 'device_id': 'd', 'session_id': 's', 'status': 'accepted', 'requested_value': 12.5,
       'requested_at_ms': 1000, 'deadline_ms': 21000, 'sent_at_ms': 1100, 'runtime_id': 'R1', 'observed_at_ms': 1200,
       'post_scan_us': 5_000_000, 'scan_anchor_ms': 1000, 'registry_sha256': 'r' * 64}
AS_OF = 8000          # ms wall -> 7_000_000 us in the scan clock


def sample(t=6_000_000, value=12.5, stale=False, healthy=True, rid='R1', wd=False, fault=None, forces=(), latch=None, scan=100, wall=None):
    return {'event_id': 'e%d' % t, 'native_runtime_id': rid, 'scan_count': scan, 'scan_timestamp_us': t,
            'observed_at_ms': wall if wall is not None else t // 1000, 'collected_at_ms': t // 1000,
            'variables': [{'name': 'sp_flow_fb', 'type_name': 'REAL', 'decoded_value': value,
                           'input': {'device': 'coupler', 'channel': 'sp_echo', 'stale': stale}}],
            'device_health': [{'name': 'coupler', 'healthy': healthy}], 'watchdog_tripped': wd, 'fault': fault,
            'latch_reason': latch, 'forces': list(forces)}


def derive(samples, events=(ACC,), as_of=AS_OF, request=REQ, entry=ENTRY, sha=None):
    return verifier.derive(request, entry, list(events), list(samples), as_of, registry_sha256=sha)


class Verifier(unittest.TestCase):
    def test_verified_with_references(self):
        r = derive([sample()])
        self.assertEqual(r['status'], 'verified')
        self.assertEqual(r['observation_refs']['variable'], 'sp_flow_fb')
        self.assertEqual(r['observation_refs']['scan_timestamp_us'], 6_000_000)
        self.assertIn('not_claimed', r)

    def test_not_verified(self):
        cases = {
            'E1 stale feedback equal to target': [sample(stale=True)],
            'device unhealthy': [sample(healthy=False)],
            'E2 scan time before the write': [sample(t=4_900_000)],
            'scan time exactly at the post-write marker': [sample(t=5_000_000)],
            'after the settle window': [sample(t=10_000_001)],
            'other runtime instance': [sample(rid='R2')],
            'E7 watchdog tripped in the same sample': [sample(wd=True)],
            'fault in the same sample': [sample(fault='x')],
            'latched': [sample(latch='watchdog')],
            'target forced': [sample(forces=[{'name': 'sp_flow'}])],
            'unreadable force entry': [sample(forces=[{'odd': 1}])],
            'outside tolerance': [sample(value=12.6)],
            'sample from the future of the evaluation': [sample(t=7_600_000)],
            'no samples': [],
        }
        for name, samples in cases.items():
            self.assertNotEqual(derive(samples)['status'], 'verified', name)

    def test_tolerance_edge_is_inclusive(self):
        self.assertEqual(derive([sample(value=12.55)])['status'], 'verified')
        self.assertNotEqual(derive([sample(value=12.5500001)])['status'], 'verified')

    # ---- W1: only the bridge's event is trusted, never the request record --------------------------------------------------
    def test_W1_a_forged_request_record_changes_nothing(self):
        forged = {'action_id': AID, 'target': 40.0, 'deadline_ms': 10 ** 12, 'requested_at_ms': 0, 'device_id': 'x', 'session_id': 'y'}
        self.assertNotEqual(derive([sample(value=40.0)], request=forged)['status'], 'verified', 'feedback 40 must not prove a request for 12.5')
        self.assertEqual(derive([sample(value=12.5)], request=forged)['status'], 'verified')
        self.assertEqual(derive([sample(value=12.5)], request=AID)['status'], 'verified', 'the action id alone is enough')

    def test_W1_the_registry_the_bridge_used_must_be_the_one_the_read_side_holds(self):
        self.assertEqual(derive([sample()], sha='r' * 64)['status'], 'verified')
        r = derive([sample()], sha='s' * 64)
        self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'))

    # ---- W5 / W13: ordering is in the scan clock; the sampler's wall-clock estimate is irrelevant -----------------------------
    def test_W5_a_skewed_sampler_wall_clock_cannot_make_an_old_sample_new(self):
        self.assertNotEqual(derive([sample(t=4_000_000, wall=9_999_999)])['status'], 'verified')

    def test_W5_nor_can_it_make_a_new_sample_look_old(self):
        self.assertEqual(derive([sample(t=6_000_000, wall=1)])['status'], 'verified')

    def test_the_deadline_is_mapped_into_the_scan_clock(self):
        ev = dict(ACC, deadline_ms=1000 + 5500)            # deadline at scan 5_500_000 us: tighter than the settle window
        self.assertEqual(derive([sample(t=5_400_000)], events=[ev])['status'], 'verified')
        self.assertNotEqual(derive([sample(t=5_600_000)], events=[ev])['status'], 'verified')

    # ---- W12: the first word counts ----------------------------------------------------------------------------------------
    def test_W12_a_later_contradictory_event_is_ignored(self):
        later = dict(ACC, status='rejected', reason='expired', observed_at_ms=1300)
        self.assertEqual(derive([sample()], events=[ACC, later])['status'], 'verified')
        self.assertEqual(derive([sample()], events=[later, ACC])['status'], 'verified', 'order in the list is irrelevant, observed_at_ms decides')

    def test_E6_a_verified_claim_by_anyone_counts_for_nothing(self):
        forged = dict(ACC, status='verified', observed_at_ms=1300)
        self.assertEqual(derive([], events=[forged])['status'], 'requested')
        self.assertEqual(derive([sample(stale=True)], events=[ACC, forged])['status'], 'accepted')

    def test_E3_no_feedback_channel_never_verifies(self):
        entry = dict(ENTRY, verify=None)
        self.assertEqual(derive([sample()], entry=entry, as_of=3000)['status'], 'accepted')
        r = derive([sample()], entry=entry, as_of=22000)
        self.assertEqual((r['status'], r['reason']), ('unknown', 'no_feedback'))

    def test_bridge_terminal_states_are_final(self):
        for status in ('rejected', 'modified', 'undelivered', 'unknown'):
            ev = dict(ACC, status=status, reason='x')
            self.assertEqual(derive([sample()], events=[ev])['status'], status)

    def test_other_actions_do_not_count(self):
        self.assertEqual(derive([sample()], events=[dict(ACC, action_id='b' * 32)])['status'], 'requested')

    def test_unknown_after_the_window_and_the_reason(self):
        r = derive([sample(value=1.0)], as_of=1000 + 10_000 + 1001)
        self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'))
        r = derive([sample(stale=True)], as_of=1000 + 10_000 + 1001)
        self.assertEqual((r['status'], r['reason']), ('unknown', 'feedback_stale'))
        self.assertEqual(derive([sample(value=1.0)], as_of=AS_OF)['status'], 'accepted')

    def test_C8_restarted_runtime_cannot_verify_and_is_reported(self):
        r = derive([sample(t=6_000_000, rid='R2')])
        self.assertEqual((r['status'], r['reason']), ('unknown', 'runtime_restarted'))
        self.assertEqual(derive([sample(t=6_000_000, rid='R1'), sample(t=6_500_000, rid='R2')])['status'], 'verified')
        self.assertEqual(derive([sample(t=1_000_000, rid='R2')])['status'], 'accepted', 'a foreign instance seen BEFORE the send says nothing')

    def test_accepted_without_a_scan_marker_can_never_be_verified(self):
        ev = {k: v for k, v in ACC.items() if k not in ('post_scan_us', 'scan_anchor_ms')}
        r = derive([sample()], events=[ev])
        self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'))

    def test_reconcile_note_is_attached_but_changes_nothing(self):
        n1 = {'kind': 'reconcile_note', 'action_id': AID, 'pass_no': 1, 'landed': False, 'final': False, 'detail': 'first'}
        n2 = {'kind': 'reconcile_note', 'action_id': AID, 'pass_no': 2, 'landed': True, 'final': True, 'detail': 'second'}
        ev = dict(ACC, status='unknown', reason='runtime_unreachable')
        r = derive([sample()], events=[ev, n1, n2])
        self.assertEqual((r['status'], r['reconcile']['landed'], r['reconcile']['final']), ('unknown', True, True))

    def test_a_target_the_device_already_holds_verifies_without_the_write_being_the_cause(self):
        # Found by C4: a request for the value the feedback already shows (fresh, healthy, after the write) verifies.  That is the
        # contract's rule; the claim's not_claimed says the write is not shown to be the cause.
        r = derive([sample(t=4_000_000, value=12.5), sample(t=6_200_000, value=12.5)])
        self.assertEqual(r['status'], 'verified')
        self.assertIn('原因', r['not_claimed'])

    def test_bool_feedback(self):
        ev = dict(ACC, requested_value=True)
        s = sample()
        s['variables'][0]['decoded_value'] = True
        self.assertEqual(derive([s], events=[ev], entry=dict(ENTRY, type='BOOL'))['status'], 'verified')


if __name__ == '__main__':
    unittest.main()
