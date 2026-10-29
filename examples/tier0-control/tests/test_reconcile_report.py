"""WP6 three-way reconciliation (tools/reconcile_report.py) on hand-built journals, /audit archives and device logs: each class of
CONTRACT 8.3 plus the R3T1 lesson (a write applied after its client went away, with no /audit entry), ring turnover, restarts,
unknown writes, unexplained device writes and resends."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import reconcile_report as rr  # noqa: E402

T0 = 1800000000.0
RID, RID2 = '100-1', '200-2'
DEVICE_MAP = {'sp_flow': {'device': '1', 'kind': 'f32', 'addr': 100}, 'lamp': {'device': '1', 'kind': 'coil', 'addr': 0},
              'prog_sp': {'device': '1', 'kind': 'f32', 'addr': 130, 'program_value': 1069547520}}     # the program assigns 1.5
BITS_12_5 = 1095237632                     # REAL 12.5 -> words 0x4148 0x0000


def words(bits):
    u = bits & 0xFFFFFFFF
    return [u >> 16, u & 0xFFFF]


class Lab:
    def __init__(self, tc):
        tmp = tempfile.TemporaryDirectory()
        tc.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.journal, self.polls, self.slave, self.wire, self.life = [], [], [], [], []

    # -- journal ----------------------------------------------------------------------------------------------------------------
    def rec(self, kind, t, **fields):
        self.journal.append(dict(fields, kind=kind, t_ms=int(t * 1000)))

    def write(self, aid, t, var='sp_flow', bits=BITS_12_5, rid=RID, status=200, error=None, terminal=('accepted', None)):
        label = 'tier0.lab.%s' % aid[:8]
        self.rec('intent', t, action_id=aid, label=label, variable=var, bits=bits, runtime_id=rid, target_key=var)
        if status is not None:
            self.rec('response', t + 0.01, action_id=aid, http={'kind': 'response', 'status': status})
        elif error:
            self.rec('response', t + 3.0, action_id=aid, http={'kind': 'error', 'error_kind': error})
        if terminal:
            self.rec('terminal', t + 3.1, action_id=aid, event={'status': terminal[0], 'reason': terminal[1]})
        return label

    # -- other sources ----------------------------------------------------------------------------------------------------------
    def poll(self, t, entries, rid=RID, rid_after=None):
        self.polls.append({'t': t, 'runtime_id': rid, 'runtime_id_after': rid if rid_after is None else rid_after, 'audit': entries})

    def polls_every(self, t0, t1, entries_at, rid=RID):
        t = t0
        while t <= t1:
            self.poll(t, entries_at(t), rid)
            t += 1.0

    def device(self, t, var, bits, action='resp'):
        m = DEVICE_MAP[var]
        r = {'ev': 'req', 'action': action, 't_req': t, 'addr': m['addr']}
        r.update({'fc': 5, 'value': 0xFF00 if bits else 0} if m['kind'] == 'coil' else {'fc': 16, 'words': words(bits), 'value': words(bits)[0]})
        self.slave.append(r)

    def post(self, t, label):
        self.wire.append({'ev': 'req', 'method': 'POST', 'path': '/write', 'origin': label, 't': t})

    # -- run --------------------------------------------------------------------------------------------------------------------
    def files(self, chain_break=False):
        prev, lines = '0' * 64, []
        for n, r in enumerate(self.journal, 1):
            raw = json.dumps(dict(r, seq=n, prev=prev), sort_keys=True, separators=(',', ':')).encode('ascii')
            lines.append(raw)
            prev = hashlib.sha256(raw).hexdigest()
        if chain_break and len(lines) > 1:
            lines[1] = lines[1].replace(b'"seq":2', b'"seq":9')
        (self.dir / 'journal.jsonl').write_bytes(b''.join(l + b'\n' for l in lines))
        for name, rows in (('audit.jsonl', self.polls), ('slave-1.jsonl', self.slave), ('wire.jsonl', self.wire),
                           ('life.jsonl', self.life)):
            (self.dir / name).write_text(''.join(json.dumps(r) + '\n' for r in rows))
        (self.dir / 'map.json').write_text(json.dumps(DEVICE_MAP))
        return self.dir

    def run(self, wire=False, lifecycle=False, chain_break=False):
        d = self.files(chain_break)
        return rr.reconcile(str(d / 'journal.jsonl'), str(d / 'audit.jsonl'), str(d / 'map.json'), {'1': str(d / 'slave-1.jsonl')},
                            str(d / 'life.jsonl') if lifecycle else None, str(d / 'wire.jsonl') if wire else None)


def entry(label, var='sp_flow', bits=BITS_12_5, applied=BITS_12_5, result='ok', ts=int(T0)):
    e = {'origin': label, 'op': 'write', 'name': var, 'requested': bits, 'result': result, 'ts_unix_secs': ts}
    if applied is not None:
        e['applied'] = applied
    return e


def cls(report, aid):
    return next(r['class'] for r in report['actions'] if r['action_id'] == aid)


class Reconcile(unittest.TestCase):
    def test_journal_audit_and_device_agree(self):
        lab = Lab(self)
        label = lab.write('a' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(rep['verdict'], 'CLEAN', rep)
        self.assertEqual(cls(rep, 'a' * 32), 'consistent')
        self.assertEqual(rep['actions'][0]['device'], 'seen')

    def test_write_applied_after_its_client_left_without_an_entry_is_explained(self):
        """R3T1 / R3T1X: no HTTP answer, the device got the value, the archive covers the instance and has no entry."""
        lab = Lab(self)
        lab.write('b' * 32, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.device(T0 + 5.0, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 30, lambda t: [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'b' * 32), 'executed_unaudited')
        self.assertEqual(rep['verdict'], 'CLEAN', rep['unexplained'])

    def test_an_http_answer_without_an_entry_is_a_discrepancy(self):
        lab = Lab(self)
        lab.write('c' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 30, lambda t: [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'c' * 32), 'unexplained')
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_an_http_answer_without_an_entry_or_a_device_write_is_a_discrepancy(self):
        lab = Lab(self)
        lab.write('8' * 32, T0 + 1, status=409, terminal=('rejected', 'latched'))
        lab.polls_every(T0, T0 + 30, lambda t: [])
        rep = lab.run()
        self.assertEqual(cls(rep, '8' * 32), 'unexplained')
        self.assertIn('HTTP 409', rep['unexplained'][0]['problems'][0])

    def test_two_entries_for_one_label_are_a_discrepancy_even_without_the_wire_log(self):
        lab = Lab(self)
        label = lab.write('9' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label), entry(label, ts=int(T0) + 3)] if t >= T0 + 4 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, '9' * 32), 'unexplained')
        self.assertIn('2 audit entries', ' '.join(rep['unexplained'][0]['problems']))

    def test_an_entry_no_intent_explains_is_an_unknown_write(self):
        lab = Lab(self)
        lab.polls_every(T0, T0 + 5, lambda t: [entry('tier0.lab.ffffffff')] if t >= T0 + 2 else [])
        lab.device(T0 + 1.5, 'sp_flow', BITS_12_5)
        rep = lab.run()
        self.assertEqual(rep['counts']['unknown_writes'], 1)
        self.assertEqual(rep['counts']['unexplained_device_writes'], 1)
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_a_wrapped_ring_read_in_time_loses_nothing(self):
        lab = Lab(self)
        labels = [lab.write('%08x' % i + 'a' * 24, T0 + 1 + i * 0.01, var='lamp', bits=i % 2, terminal=None) for i in range(300)]
        ring = [entry(l, var='lamp', bits=i % 2, applied=i % 2) for i, l in enumerate(labels)]
        lab.device(T0 + 1 + len(labels) * 0.01, 'lamp', (len(labels) - 1) % 2)   # the newest value reaches the coil
        for n in range(0, 301, 100):                                  # reads after 0, 100, 200 and 300 entries: never 256 apart
            lab.poll(T0 + 10 + n / 100.0, ring[:n][-rr.RING_CAP:])
        lab.poll(T0 + 30, ring[-rr.RING_CAP:])
        rep = lab.run()
        self.assertEqual(rep['audit_archive']['entries'], 300)
        self.assertEqual(rep['audit_archive']['turnover_gaps'], [])
        self.assertEqual(rep['counts'].get('consistent'), 300, rep['unexplained'][:2])

    def test_a_ring_that_turned_over_between_reads_makes_the_window_uncheckable(self):
        lab = Lab(self)
        labels = [lab.write('%08x' % i + 'a' * 24, T0 + 1 + i * 0.01, var='lamp', bits=i % 2, terminal=None) for i in range(600)]
        ring = [entry(l, var='lamp', bits=i % 2, applied=i % 2) for i, l in enumerate(labels)]
        lab.device(T0 + 1 + len(labels) * 0.01, 'lamp', (len(labels) - 1) % 2)   # the newest value reaches the coil
        lab.poll(T0 + 1.5, ring[:10])
        lab.poll(T0 + 20, ring[-rr.RING_CAP:])                     # 590 new entries since the last read: some are gone
        lab.poll(T0 + 30, ring[-rr.RING_CAP:])
        rep = lab.run()
        self.assertEqual(len(rep['audit_archive']['turnover_gaps']), 1)
        lost = [r for r in rep['actions'] if r['class'] == 'uncheckable_turnover']
        self.assertEqual(len(lost), 600 - 10 - rr.RING_CAP)
        self.assertEqual(rep['verdict'], 'CLEAN', rep['unexplained'][:2])

    def test_one_read_of_a_full_ring_cannot_vouch_for_what_came_before_it(self):
        """A4: 300 writes, one /audit read at the end.  The 44 oldest are uncheckable -- neither "not written" nor "consistent"."""
        lab = Lab(self)
        labels = [lab.write('%08x' % i + 'b' * 24, T0 + 1 + i * 0.01, var='lamp', bits=i % 2, terminal=None) for i in range(300)]
        lab.device(T0 + 4.5, 'lamp', 1)
        ring = [entry(l, var='lamp', bits=i % 2, applied=i % 2) for i, l in enumerate(labels)]
        lab.poll(T0 + 20, ring[-rr.RING_CAP:])
        rep = lab.run()
        self.assertEqual(rep['counts'].get('uncheckable_turnover'), 44)
        self.assertEqual(rep['counts'].get('consistent'), 256)
        self.assertEqual(rep['audit_archive']['turnover_gaps'], [{'runtime_id': RID, 'after': None, 'before': T0 + 20}])
        self.assertEqual(rep['verdict'], 'CLEAN')

    def test_a_restart_before_the_archive_saw_the_write_is_uncheckable_and_the_new_instance_boot_write_is_explained(self):
        lab = Lab(self)
        lab.write('d' * 32, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.device(T0 + 1.2, 'sp_flow', BITS_12_5)                    # applied, then the instance died before the next read
        lab.polls_every(T0, T0 + 1, lambda t: [])
        lab.life += [{'ev': 'runtime_ready', 't': T0, 'spawned_t': T0 - 1, 'runtime_id': RID},
                     {'ev': 'runtime_ready', 't': T0 + 4, 'spawned_t': T0 + 2, 'runtime_id': RID2}]
        lab.device(T0 + 2.5, 'sp_flow', 0)                            # the new instance writes its initial output before it is ready
        lab.polls_every(T0 + 4, T0 + 20, lambda t: [], rid=RID2)
        rep = lab.run(lifecycle=True)
        self.assertEqual(cls(rep, 'd' * 32), 'uncheckable_restart')
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained'], rep['unexplained_device_writes']))
        without = lab.run(lifecycle=False)                            # without the spawn time the boot write looks like the old instance's
        self.assertEqual(without['counts']['unexplained_device_writes'], 1)

    def test_a_device_value_nothing_asked_for_is_unexplained(self):
        lab = Lab(self)
        lab.polls_every(T0, T0 + 5, lambda t: [])
        lab.device(T0 + 0.5, 'sp_flow', 0)                            # initial output: explained
        lab.device(T0 + 2.0, 'sp_flow', 777)
        rep = lab.run()
        self.assertEqual([d['bits'] for d in rep['unexplained_device_writes']], [777])
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_the_program_s_own_output_and_a_safe_state_explain_device_writes(self):
        lab = Lab(self)
        label = lab.write('7' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.device(T0 + 0.5, 'prog_sp', 1069547520)                 # the program's 1.5, written at start and again on reconnect
        lab.device(T0 + 9.0, 'prog_sp', 1069547520)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
        lab.device(T0 + 15.0, 'sp_flow', 0)                           # a latch or a stop zeroes the outputs
        lab.device(T0 + 15.0, 'prog_sp', 0)
        lab.life += [{'ev': 'runtime_ready', 't': T0, 'spawned_t': T0 - 1, 'runtime_id': RID}]
        before = lab.run(lifecycle=True)
        # prog_sp's 0 passes anyway: nothing ever asked the instance for another value, so 0 is still its initial output
        self.assertEqual([d['variable'] for d in before['unexplained_device_writes']], ['sp_flow'])
        lab.life.append({'ev': 'safe_state', 't': T0 + 14.9, 'why': 'watchdog latch injected'})
        rep = lab.run(lifecycle=True)
        self.assertEqual(rep['verdict'], 'CLEAN', rep['unexplained_device_writes'])
        lab.device(T0 + 16.0, 'prog_sp', 1069547521)                  # not the program's value, not failsafe
        self.assertEqual(lab.run(lifecycle=True)['counts']['unexplained_device_writes'], 1)

    def test_two_posts_with_one_label_are_a_resend(self):
        lab = Lab(self)
        label = lab.write('e' * 32, T0 + 1)
        lab.post(T0 + 1.0, label)
        lab.post(T0 + 4.0, label)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run(wire=True)
        self.assertEqual(cls(rep, 'e' * 32), 'unexplained')
        self.assertIn('resend', ' '.join(rep['unexplained'][0]['problems']))

    def test_an_intent_the_bridge_never_sent(self):
        lab = Lab(self)
        lab.write('f' * 32, T0 + 1, status=None, terminal=('unknown', 'bridge_restarted'))
        lab.polls_every(T0, T0 + 20, lambda t: [])
        rep = lab.run(wire=True)
        self.assertEqual(cls(rep, 'f' * 32), 'not_sent')
        self.assertEqual(cls(lab.run(wire=False), 'f' * 32), 'no_effect_no_entry')
        lab.polls = [p for p in lab.polls if p['t'] <= T0 + 2]        # the instance ends right after: the proxy still says it never left
        self.assertEqual(cls(lab.run(wire=True), 'f' * 32), 'not_sent')
        self.assertEqual(cls(lab.run(wire=False), 'f' * 32), 'uncheckable_restart')

    def test_an_applied_value_the_device_never_got(self):
        lab = Lab(self)
        label = lab.write('1' * 32, T0 + 1)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
        self.assertEqual(lab.run()['verdict'], 'DISCREPANCY')
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5, action='swallowed')   # the runtime sent it; a silent device did not answer
        rep = lab.run()
        self.assertEqual(rep['actions'][0]['device'], 'sent_unanswered')
        self.assertEqual(rep['verdict'], 'CLEAN')

    def test_a_write_ia2_took_while_the_link_was_already_gone_and_replaced_before_it_returned(self):
        """Soak, 2026-10-29: the device closed the link 0.2 s before the write; IA2 answered 200 (it had not noticed), a newer write
        replaced the value 7.6 s later, and only the newer one reached the device when the link came back."""
        lab = Lab(self)
        newer = 1099409916
        poll = lambda t: lab.slave.append({'ev': 'req', 'action': 'resp', 'fc': 3, 'addr': 100, 'qty': 2, 't_req': t})
        for k in range(10):
            poll(T0 + k * 0.1)                                         # a working link: answered polls
        l1 = lab.write('a1' * 16, T0 + 1.2)                            # the link closed at T0 + 1.0: no poll answered after it
        l2 = lab.write('a2' * 16, T0 + 8.8, bits=newer, terminal=('undelivered', 'link_down'))
        for k in range(20):
            poll(T0 + 16.4 + k * 0.1)                                  # the link is back
        lab.device(T0 + 16.4, 'sp_flow', newer)
        res = "ok; not delivered to device 'coupler' (link down)"
        lab.polls_every(T0, T0 + 30, lambda t: ([entry(l1)] if t >= T0 + 2 else []) + ([entry(l2, bits=newer, applied=newer, result=res)] if t >= T0 + 9 else []))
        rep = lab.run()
        self.assertEqual(rep['actions'][0]['device'], 'link_down_until_replaced')
        self.assertEqual(rep['verdict'], 'CLEAN', rep['unexplained'])
        poll(T0 + 5.0)                                                 # one answered poll in between: the link worked, so the value should have gone out
        self.assertEqual(lab.run()['verdict'], 'DISCREPANCY')

    def test_an_undelivered_write_that_arrives_late_is_consistent(self):
        lab = Lab(self)
        label = lab.write('2' * 32, T0 + 1, terminal=('undelivered', 'link_down'))
        lab.device(T0 + 9.0, 'sp_flow', BITS_12_5)                    # the link came back 8 s later
        res = "ok; not delivered to device 'coupler' (link down)"
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label, result=res)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, '2' * 32), 'consistent')
        self.assertEqual(rep['verdict'], 'CLEAN', rep['unexplained_device_writes'])

    def test_a_denied_write_has_an_entry_without_applied(self):
        lab = Lab(self)
        label = lab.write('3' * 32, T0 + 1, status=403, terminal=('rejected', 'governance_denied'))
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label, applied=None, result='governance: denied')] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, '3' * 32), 'consistent')
        self.assertEqual(rep['verdict'], 'CLEAN')

    def test_a_broken_chain_is_never_clean(self):
        lab = Lab(self)
        label = lab.write('4' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run(chain_break=True)
        self.assertFalse(rep['journal']['chain_ok'])
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_a_poll_across_a_restart_is_ignored(self):
        lab = Lab(self)
        lab.poll(T0, [entry('tier0.lab.ffffffff')], rid=RID, rid_after=RID2)
        rep = lab.run()
        self.assertEqual(rep['audit_archive']['polls_ignored'], 1)
        self.assertEqual(rep['counts']['unknown_writes'], 0)

    def test_a_poll_that_read_the_same_ring_says_so(self):
        lab = Lab(self)
        label = lab.write('6' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.poll(T0 + 2, [entry(label)])
        for t in range(3, 20):
            lab.polls.append({'t': T0 + t, 'runtime_id': RID, 'runtime_id_after': RID, 'same': True})
        lab.polls.append({'t': T0 + 21, 'runtime_id': RID2, 'runtime_id_after': RID2, 'same': True})   # nothing to be the same as
        rep = lab.run()
        self.assertEqual(rep['audit_archive']['polls_used'], 18)
        self.assertEqual(rep['audit_archive']['polls_ignored'], 1)
        self.assertEqual(rep['audit_archive']['instances'][0]['last_poll'], T0 + 19)
        self.assertEqual(rep['verdict'], 'CLEAN')

    def test_command_line_exit_codes(self):
        lab = Lab(self)
        label = lab.write('5' * 32, T0 + 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
        d = lab.files()
        args = [sys.executable, '-I', '-B', str(ROOT / 'tools' / 'reconcile_report.py'), '--journal', str(d / 'journal.jsonl'),
                '--audit-archive', str(d / 'audit.jsonl'), '--device-map', str(d / 'map.json'), '--device-log', '1=%s' % (d / 'slave-1.jsonl'),
                '--out', str(d / 'report.json')]
        r = subprocess.run(args, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)['verdict'], 'CLEAN')
        self.assertIn('journal.jsonl', json.loads((d / 'report.json').read_text())['inputs'])
        lab.device(T0 + 5, 'sp_flow', 777)
        lab.files()
        self.assertEqual(subprocess.run(args, capture_output=True).returncode, 1)
        (d / 'audit.jsonl').write_text(json.dumps({'t': T0, 'runtime_id': RID, 'runtime_id_after': RID, 'audit': [entry('x')]}) + '\n'
                                       + json.dumps({'t': T0 + 1, 'runtime_id': RID, 'runtime_id_after': RID, 'audit': [entry('y')]}) + '\n')
        r = subprocess.run(args, capture_output=True, text=True)
        self.assertEqual(r.returncode, 2, r.stderr)


if __name__ == '__main__':
    unittest.main()
