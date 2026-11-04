"""R3 timing follow-up (2026-10-29): a missing /audit entry is not evidence that a write was not executed.

R3T1/R3T1X showed the runtime applying a queued write after the bridge's connection closed, without recording an /audit entry.
So the reconcile note states what /audit showed (`audit_entry`: found / absent / unavailable) and claims `landed: true` only for a
found entry; it never says `landed: false`.  Journals and trace events of older bridges (`landed: false`) are read as `absent`."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path, decode_event
import json
import os
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(os.path.dirname(HERE), 'lab'))
import robustness_cases as rc  # noqa: E402
import signing_fixtures as verifier  # noqa: E402
from bridge.engine import Engine  # noqa: E402
from bridge.journal import Journal  # noqa: E402
from signing_fixtures import TraceWriter  # noqa: E402
from test_fixes import KEYS, Timeout, close_all, line, lines, make  # noqa: E402

BITS = 1095237632                                               # 12.5 as f32 bits


def notes_of(tmp):
    return [decode_event(json.loads(l)) for l in lines(os.path.join(tmp, 't.jsonl')) if '"reconcile_note"' in l]


def shape(n):
    return n['pass_no'], n['audit_entry'], n['landed'], n['final']


def timed_out(tmp, client=None):
    client = client or Timeout()
    eng, jr, tw, t = make(tmp, client)
    ln, p = line(t)
    eng.handle_line(ln)
    ev = jr.final_event(p['action_id'])
    assert (ev['status'], ev['reason']) == ('unknown', 'runtime_unreachable')
    return eng, jr, tw, t, client, p


class AbsentEntry(unittest.TestCase):
    def test_two_looks_without_an_entry_never_claim_not_landed(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t, client, p = timed_out(tmp)
            t[0] += 5000
            eng.reconcile_unknowns()
            t[0] += 11000
            eng.reconcile_unknowns()
            notes = notes_of(tmp)
            self.assertEqual([shape(n) for n in notes], [(1, 'absent', None, False), (2, 'absent', None, True)])
            for n in notes:
                self.assertIn('does not show', n['detail'])
            self.assertIn('older runtime may have lost the audit', notes[1]['detail'],
                          'the second look distinguishes the historical queued-write audit gap')
            self.assertIn('256-entry ring may have turned over', notes[1]['detail'],
                          'missing retained evidence still cannot prove non-execution')
            recs = [r for r in jr.actions[p['action_id']]['reconciles']]
            self.assertEqual([(r['pass_no'], r['audit_entry'], r['landed']) for r in recs], [(1, 'absent', None), (2, 'absent', None)])
            t[0] += 60000
            eng.reconcile_unknowns()
            self.assertEqual(len(notes_of(tmp)), 2, 'two looks, no more')
            self.assertEqual(jr.final_event(p['action_id'])['status'], 'unknown')
            close_all(jr, tw)

    def test_no_note_ever_carries_landed_false(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t, client, p = timed_out(tmp)
            for _ in range(4):
                t[0] += 11000
                eng.reconcile_unknowns()
            raw = [l for l in lines(os.path.join(tmp, 't.jsonl')) if '"reconcile_note"' in l]
            self.assertTrue(raw)
            self.assertFalse(any('"landed":false' in l for l in raw), raw)
            close_all(jr, tw)


class FoundOrUnavailable(unittest.TestCase):
    def test_found_at_the_first_look_is_final_and_claims_landed(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t, client, p = timed_out(tmp)
            label = jr.actions[p['action_id']]['intent']['label']
            client.audit = [{'origin': label, 'name': 'sp_flow', 'requested': BITS, 'applied': BITS, 'result': 'ok'}]
            t[0] += 5000
            eng.reconcile_unknowns()
            t[0] += 11000
            eng.reconcile_unknowns()
            notes = notes_of(tmp)
            self.assertEqual([shape(n) for n in notes], [(1, 'found', True, True)])
            self.assertEqual(notes[0]['audit_result'], 'ok')
            close_all(jr, tw)

    def test_a_restarted_runtime_is_unavailable_not_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t, client, p = timed_out(tmp)
            client.status = dict(client.status, runtime_id='another-instance')
            t[0] += 5000
            eng.reconcile_unknowns()
            t[0] += 11000
            eng.reconcile_unknowns()
            self.assertEqual([shape(n) for n in notes_of(tmp)], [(1, 'unavailable', None, True)])
            close_all(jr, tw)


class LegacyRecords(unittest.TestCase):
    def test_a_legacy_first_look_still_gets_its_second_look(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t, client, p = timed_out(tmp)
            aid = p['action_id']
            jr.append('reconcile', action_id=aid, landed=False, detail='written by a bridge before v0.9', pass_no=1)
            close_all(jr, tw)
            jr2 = Journal(os.path.join(tmp, 'j.jsonl'), lambda: t[0])
            jr2.open()
            tw2 = TraceWriter(os.path.join(tmp, 't.jsonl'))
            rec_t = jr2.actions[aid]['reconciles'][0]['t_ms']
            self.assertEqual(jr2.reconcile_due(rec_t + 9999, 10000), [])
            self.assertEqual(jr2.reconcile_due(rec_t + 10000, 10000), [(aid, 2)])
            eng2 = Engine(eng.cfg, eng.registry, KEYS, jr2, client, tw2, clock=lambda: rec_t + 10000, sleep=lambda s: None)
            eng2.reconcile_unknowns()
            self.assertEqual([shape(n) for n in notes_of(tmp)], [(2, 'absent', None, True)])
            close_all(jr2, tw2)


class Crash(BaseException):
    """Stands for the bridge being killed while its POST is in flight."""


class Recovery(unittest.TestCase):
    def test_recovery_without_an_entry_does_not_read_it_as_not_executed(self):
        class Dies(rc.FakeClient):
            def request(self, method, path, body=None, origin=None):
                raise Crash()
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp, Dies())
            ln, p = line(t)
            with self.assertRaises(Crash):
                eng.handle_line(ln)
            close_all(jr, tw)
            eng2, jr2, tw2, t2 = make(tmp, rc.FakeClient(), t0=t[0] + 1000)
            eng2.recover()
            ev = jr2.final_event(p['action_id'])
            self.assertEqual((ev['status'], ev['reason']), ('unknown', 'bridge_restarted'))
            self.assertIn('does not show it was not executed', ev['detail'])
            self.assertIn('not resent', ev['detail'])
            close_all(jr2, tw2)


class ReadSide(unittest.TestCase):
    AID = 'a' * 32
    STATUS = {'kind': 'action_status', 'action_id': AID, 'status': 'unknown', 'reason': 'runtime_unreachable', 'observed_at_ms': 1}

    def derive(self, note):
        return verifier.derive(self.AID, {'type': 'REAL'}, [self.STATUS, dict(note, kind='reconcile_note', action_id=self.AID)], [], 10)

    def test_a_legacy_landed_false_note_is_read_as_absent(self):
        r = self.derive({'pass_no': 2, 'landed': False, 'final': True, 'detail': 'old'})
        self.assertEqual((r['status'], r['reconcile']['audit_entry'], r['reconcile']['landed']), ('unknown', 'absent', None))

    def test_new_notes_pass_through(self):
        for entry, landed in (('found', True), ('absent', None), ('unavailable', None)):
            r = self.derive({'pass_no': 1, 'landed': landed, 'audit_entry': entry, 'final': True})
            self.assertEqual((r['reconcile']['audit_entry'], r['reconcile']['landed']), (entry, landed))

    def test_malformed_note_fields_claim_nothing(self):
        r = self.derive({'pass_no': 1, 'landed': ['x'], 'audit_entry': {'y': 1}, 'final': True})
        self.assertEqual((r['reconcile']['audit_entry'], r['reconcile']['landed']), ('unavailable', None))
        r = self.derive({'pass_no': 1, 'landed': True, 'audit_entry': 'absent', 'final': True})
        self.assertIsNone(r['reconcile']['landed'], 'the stated entry decides, not a contradicting landed flag')


if __name__ == '__main__':
    unittest.main()
