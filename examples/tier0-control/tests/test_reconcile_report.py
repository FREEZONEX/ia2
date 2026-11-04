"""WP6 three-way reconciliation (tools/reconcile_report.py) on hand-built journals, /audit archives and device logs: each class of
CONTRACT 8.3 plus the R3T1 lesson (a write applied after its client went away, with no /audit entry), ring turnover, restarts,
unknown writes, unexplained device writes and resends."""
import hashlib
import json
import math
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
        self.tc = tc
        tmp = tempfile.TemporaryDirectory()
        tc.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.journal, self.polls, self.slave, self.wire, self.life = [], [], [], [], []
        # The device log as the lab's fake device writes it: `listen` when it starts, `stopped` when it is stopped, a `seq` on
        # every record.  Tests that need a truncated or older log change these.
        self.log_start, self.log_end, self.sequenced, self.device_map = T0 - 1, T0 + 60, True, DEVICE_MAP

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
        r = {'ev': 'req', 'action': action, 't': t, 't_req': t, 'addr': m['addr']}
        r.update({'fc': 5, 'value': 0xFF00 if bits else 0} if m['kind'] == 'coil' else {'fc': 16, 'words': words(bits), 'value': words(bits)[0]})
        self.slave.append(r)

    def answered_polls(self, t0, t1):
        """The runtime reading the device every 100 ms and the device answering: a working link."""
        for k in range(int(round((t1 - t0) * 10)) + 1):
            t = t0 + k / 10.0
            self.slave.append({'ev': 'req', 'action': 'resp', 'fc': 3, 'addr': 100, 'qty': 2, 't': t, 't_req': t})

    def mode(self, t, mode):
        self.slave.append({'ev': 'mode', 'mode': mode, 't': t})

    def device_log(self):
        rows = sorted(self.slave, key=lambda r: r.get('t', r.get('t_req')))
        if self.log_start is not None:
            rows.insert(0, {'ev': 'listen', 't': self.log_start, 'port': 13511})
        if self.log_end is not None:
            rows.append({'ev': 'stopped', 't': self.log_end})
        if self.sequenced:
            rows = [dict(r, seq=n) for n, r in enumerate(rows, 1)]
        return rows

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
        for name, rows in (('audit.jsonl', self.polls), ('slave-1.jsonl', self.device_log()), ('wire.jsonl', self.wire),
                           ('life.jsonl', self.life)):
            (self.dir / name).write_text(''.join(json.dumps(r) + '\n' for r in rows))
        (self.dir / 'map.json').write_text(json.dumps(self.device_map))
        return self.dir

    def run(self, wire=False, lifecycle=False, chain_break=False):
        d = self.files(chain_break)
        return reconcile_or_fail(self.tc, d, lifecycle=str(d / 'life.jsonl') if lifecycle else None,
                                 wire=str(d / 'wire.jsonl') if wire else None)


def reconcile_or_fail(tc, d, **kw):
    """A crash of the program under test is a failed test, not a test error: the vectors judge what the program does."""
    try:
        return rr.reconcile(str(d / 'journal.jsonl'), str(d / 'audit.jsonl'), str(d / 'map.json'), {'1': str(d / 'slave-1.jsonl')}, **kw)
    except rr.InputError:
        raise
    except Exception as exc:                                          # noqa: BLE001
        tc.fail('reconcile crashed: %s: %s' % (type(exc).__name__, exc))


def assert_refused(tc, d):
    try:
        rr.reconcile(str(d / 'journal.jsonl'), str(d / 'audit.jsonl'), str(d / 'map.json'), {'1': str(d / 'slave-1.jsonl')})
    except rr.InputError as exc:
        return str(exc)
    except Exception as exc:                                          # noqa: BLE001
        tc.fail('unusable input should be refused, the program crashed: %s: %s' % (type(exc).__name__, exc))
    tc.fail('unusable input was accepted')


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
        # Without the spawn time both device writes fall after the archive last read the old instance (T0 + 1) and before it
        # first read the new one (T0 + 4): 12.5 then 0 is explained only if the handover came between them (old's value, then
        # new's initial output).  An evidence gap, not a discrepancy -- and a value neither could have written is still one.
        without = lab.run(lifecycle=False)
        self.assertEqual((without['counts']['unexplained_device_writes'], without['counts']['unchecked_device_writes']), (0, 2))
        self.assertEqual([d.get('possible_instances') for d in without['unchecked_device_writes']], [[RID, RID2]] * 2)
        self.assertEqual(without['verdict'], 'INCOMPLETE')
        lab.device(T0 + 3.0, 'sp_flow', 777)
        without = lab.run(lifecycle=False)
        self.assertEqual([d['bits'] for d in without['unexplained_device_writes']], [777])

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
        lab.polls = [p for p in lab.polls if p['t'] <= T0 + 2]        # the archive stops seeing the instance right after
        self.assertEqual(cls(lab.run(wire=True), 'f' * 32), 'not_sent')       # the proxy still says it never left
        rep = lab.run(wire=False)
        self.assertEqual(cls(rep, 'f' * 32), 'uncheckable_archive')   # nothing says the instance ended: the archive just stops
        self.assertEqual(rep['verdict'], 'INCOMPLETE')
        lab.polls_every(T0 + 6, T0 + 20, lambda t: [], rid=RID2)      # a later instance: the first one ended
        self.assertEqual(cls(lab.run(wire=False), 'f' * 32), 'uncheckable_restart')

    def test_an_applied_value_the_device_never_got(self):
        lab = Lab(self)
        label = lab.write('1' * 32, T0 + 1)
        lab.answered_polls(T0, T0 + 20)                               # the device answered the runtime throughout
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
        lab.poll(T0 - 1, [])
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


NEWER = 1099409916                         # REAL 15.5


def device_row(report, aid):
    return next(r for r in report['actions'] if r['action_id'] == aid)


class Replacement(unittest.TestCase):
    """REPAIR-ADVICE FIX01: only a later write that /audit shows applied, in the same runtime instance, after this one in the ring,
    can explain why this write's value never reached a device that was answering."""

    def first_write_never_delivered(self, lab):
        """sp_flow 12.5 applied and audited `ok`, the device answering the runtime throughout, the value never on the device."""
        lab.answered_polls(T0, T0 + 30)
        return lab.write('a1' * 16, T0 + 1)

    def test_a_denied_later_write_explains_nothing(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('b2' * 16, T0 + 1.5, bits=NEWER, status=403, terminal=('rejected', 'governance_denied'))
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2, bits=NEWER, applied=None, result='governance: denied')]
                        if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_a_later_write_refused_with_its_answer_lost_explains_nothing(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('b1' * 16, T0 + 1.5, bits=NEWER, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2, bits=NEWER, applied=None, result='governance: denied')]
                        if t >= T0 + 2 else [])
        rep = lab.run()                                               # only the entry says it was refused; nothing was applied
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))

    def test_a_later_intent_the_bridge_never_sent_explains_nothing(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        lab.post(T0 + 1, l1)
        lab.write('b3' * 16, T0 + 1.5, bits=NEWER, status=None, terminal=('unknown', 'bridge_restarted'))
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1)] if t >= T0 + 2 else [])
        rep = lab.run(wire=True)
        self.assertEqual(cls(rep, 'b3' * 16), 'not_sent')
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))

    def test_a_later_write_that_timed_out_with_no_entry_and_no_effect_explains_nothing(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        lab.write('b4' * 16, T0 + 1.5, bits=NEWER, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))

    def test_a_confirmed_later_write_in_the_same_instance_and_output_cycle_explains_it(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('b5' * 16, T0 + 1.5, bits=NEWER, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.device(T0 + 1.55, 'sp_flow', NEWER)                       # only the newer value went out
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2, bits=NEWER, applied=NEWER)] if t >= T0 + 2 else [])
        rep = lab.run()                                               # an audited apply is proof even without an HTTP answer
        self.assertEqual(device_row(rep, 'a1' * 16).get('device'), 'replaced_before_delivery', rep['unexplained'])
        self.assertEqual(device_row(rep, 'a1' * 16).get('replaced_by'), 'b5' * 16)
        self.assertEqual(rep['verdict'], 'CLEAN', rep['unexplained'])

    def test_a_clamped_later_write_is_checked_with_the_value_it_applied(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        clamped = 1092616192                                          # REAL 10.0: what governance let through
        l2 = lab.write('b6' * 16, T0 + 1.5, bits=NEWER)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2, bits=NEWER, applied=clamped, result='ok (clamped)')]
                        if t >= T0 + 2 else [])
        lab.device(T0 + 1.55, 'sp_flow', clamped)
        rep = lab.run()
        self.assertEqual(device_row(rep, 'a1' * 16).get('device'), 'replaced_before_delivery')
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained'], rep['unexplained_device_writes']))
        lab.slave = [r for r in lab.slave if r.get('fc') != 16]
        lab.device(T0 + 1.55, 'sp_flow', NEWER)                       # the requested value, which nothing applied
        rep = lab.run()
        self.assertEqual([d['bits'] for d in rep['unexplained_device_writes']], [NEWER])

    def test_writing_the_same_value_again_does_not_end_the_duty_to_deliver_it(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('b7' * 16, T0 + 1.5)                           # 12.5 again, applied again, still never delivered
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))
        self.assertEqual(cls(rep, 'b7' * 16), 'unexplained')

    def test_a_write_to_another_variable_explains_nothing(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('b8' * 16, T0 + 1.5, var='lamp', bits=1)
        lab.device(T0 + 1.55, 'lamp', 1)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2, var='lamp', bits=1, applied=1)] if t >= T0 + 2 else [])
        self.assertEqual(cls(lab.run(), 'a1' * 16), 'unexplained')

    def test_the_ring_order_is_not_the_apply_order(self):
        """Re-review R2: IA2 records an entry after the scan thread applied and acknowledged the write, in whatever order the
        request tasks get the audit lock, so the ring does not say which write came last.  12.5 (no answer, entry first seen at
        T0 + 2) and 15.5 (sent at T0 + 1.5, answered at T0 + 1.51) may have been applied either way round; only 15.5 reached
        the device.  12.5 was merged if it came first, and should have gone out if it came last: not decided by guessing."""
        for ring in ('newer first', 'older first'):
            with self.subTest(ring):
                lab = Lab(self)
                lab.answered_polls(T0, T0 + 30)
                l1 = lab.write('a1' * 16, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
                l2 = lab.write('b9' * 16, T0 + 1.5, bits=NEWER)
                lab.device(T0 + 1.55, 'sp_flow', NEWER)
                both = [entry(l2, bits=NEWER, applied=NEWER), entry(l1)]
                lab.polls_every(T0, T0 + 30, lambda t: (both if ring == 'newer first' else both[::-1]) if t >= T0 + 2 else [])
                rep = lab.run()
                self.assertEqual(device_row(rep, 'a1' * 16).get('device'), 'uncheckable_order', rep['unexplained'])
                self.assertEqual(device_row(rep, 'a1' * 16).get('unordered_with'), ['b9' * 16])
                self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_the_bridge_s_own_answers_fix_the_order(self):
        """The order is known when one write's interval ends before the other's begins: 12.5's entry was first seen at T0 + 2,
        before 15.5 was sent -- 15.5 came last and replaced 12.5 within the output cycle.  Sent after 15.5's answer instead, 12.5
        came last and should have reached the device."""
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        l1 = lab.write('a1' * 16, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        l2 = lab.write('b9' * 16, T0 + 2.5, bits=NEWER)
        lab.device(T0 + 2.55, 'sp_flow', NEWER)
        lab.polls_every(T0, T0 + 30, lambda t: ([entry(l1)] if t >= T0 + 2 else []) + ([entry(l2, bits=NEWER, applied=NEWER)]
                                                                                       if t >= T0 + 3 else []))
        rep = lab.run()
        self.assertEqual(device_row(rep, 'a1' * 16).get('device'), 'replaced_before_delivery', rep['unexplained'])
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained'], rep['unchecked_device_writes']))
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        l2 = lab.write('b9' * 16, T0 + 1.5, bits=NEWER)
        l1 = lab.write('a1' * 16, T0 + 1.6, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.device(T0 + 1.55, 'sp_flow', NEWER)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), entry(l2, bits=NEWER, applied=NEWER)] if t >= T0 + 2 else [])
        rep = lab.run()                                               # the ring says 12.5 first; the bridge's answers say otherwise
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))

    def test_a_later_write_whose_entry_contradicts_it_explains_nothing(self):
        other = 1092616192
        for name, status, ent in (('HTTP 409 yet applied', 409, lambda l: entry(l, bits=NEWER, applied=NEWER)),
                                  ('entry for another value', 200, lambda l: entry(l, bits=other, applied=other))):
            with self.subTest(name):
                lab = Lab(self)
                l1 = self.first_write_never_delivered(lab)
                l2 = lab.write('c3' * 16, T0 + 1.5, bits=NEWER, status=status)
                lab.polls_every(T0, T0 + 30, lambda t: [entry(l1), ent(l2)] if t >= T0 + 2 else [])
                rep = lab.run()
                self.assertEqual(cls(rep, 'c3' * 16), 'unexplained')
                self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))

    def test_a_write_the_next_instance_executed_does_not_replace_the_old_instance_s_value(self):
        """No lifecycle log: the bridge sent to the instance it knew, the next instance executed it (the entry is in that
        instance's ring), and the archive first saw the next instance later."""
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('c4' * 16, T0 + 5.5, bits=NEWER)
        lab.device(T0 + 5.6, 'sp_flow', NEWER)
        lab.polls_every(T0, T0 + 5, lambda t: [entry(l1)] if t >= T0 + 2 else [])
        lab.polls_every(T0 + 8, T0 + 30, lambda t: [entry(l2, bits=NEWER, applied=NEWER)], rid=RID2)
        rep = lab.run()
        self.assertEqual(device_row(rep, 'c4' * 16).get('note', '')[:20], 'executed by the next')
        # The archive read the old instance until T0 + 5, four seconds after the write, the device answering throughout.
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        l2 = lab.write('c4' * 16, T0 + 2.5, bits=NEWER)               # applied by the next instance within the output cycle
        lab.device(T0 + 2.6, 'sp_flow', NEWER)
        lab.polls_every(T0, T0 + 2, lambda t: [entry(l1)] if t >= T0 + 2 else [])
        lab.polls_every(T0 + 8, T0 + 30, lambda t: [entry(l2, bits=NEWER, applied=NEWER)], rid=RID2)
        rep = lab.run()
        # Last read at T0 + 2: the old instance may have ended within one output cycle, or lived on; nothing records the
        # handover, and the next instance's write explains nothing about it.
        self.assertEqual(cls(rep, 'a1' * 16), 'uncheckable_order', device_row(rep, 'a1' * 16))
        self.assertEqual(device_row(rep, 'a1' * 16).get('handover'), [T0 + 2, T0 + 8])

    def test_a_write_in_the_next_instance_does_not_replace_a_value_of_the_old_one(self):
        lab = Lab(self)
        l1 = self.first_write_never_delivered(lab)
        lab.life += [{'ev': 'runtime_ready', 't': T0 - 1, 'spawned_t': T0 - 2, 'runtime_id': RID},
                     {'ev': 'runtime_ready', 't': T0 + 6, 'spawned_t': T0 + 5.5, 'runtime_id': RID2}]
        l2 = lab.write('c1' * 16, T0 + 7, bits=NEWER, rid=RID2)
        lab.device(T0 + 5.6, 'sp_flow', 0)                            # the new instance's initial output
        lab.device(T0 + 7.05, 'sp_flow', NEWER)
        lab.polls_every(T0, T0 + 5, lambda t: [entry(l1)] if t >= T0 + 2 else [])
        lab.polls_every(T0 + 6, T0 + 30, lambda t: [entry(l2, bits=NEWER, applied=NEWER)] if t >= T0 + 8 else [], rid=RID2)
        rep = lab.run(lifecycle=True)
        # The old instance lived 4.5 s after the write with the device answering, and never sent 12.5.
        self.assertEqual(cls(rep, 'a1' * 16), 'unexplained', device_row(rep, 'a1' * 16))
        lab.life[1] = dict(lab.life[1], spawned_t=T0 + 1.6)          # the instance ended within the output cycle: explained
        lab.slave = [r for r in lab.slave if not (r.get('fc') == 16 and r['words'] == [0, 0])]
        lab.device(T0 + 1.7, 'sp_flow', 0)
        rep = lab.run(lifecycle=True)
        self.assertEqual(device_row(rep, 'a1' * 16).get('device'), 'replaced_before_delivery', rep['unexplained'])
        self.assertEqual(device_row(rep, 'a1' * 16).get('replaced_by'), 'restart')
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained'], rep['unexplained_device_writes']))


class DeviceLogCoverage(unittest.TestCase):
    """REPAIR-ADVICE FIX02: "no answered request" proves no working link only over a stretch the device log demonstrably covers,
    and a missing, cut-off or damaged log is an evidence gap, never a clean result."""

    def replaced_after_a_quiet_stretch(self, lab):
        """12.5 applied at T0 + 1.2; the device answers nothing after T0 + 1.0; 15.5 is confirmed applied at T0 + 8.8."""
        lab.answered_polls(T0, T0 + 0.9)
        l1 = lab.write('d1' * 16, T0 + 1.2)
        l2 = lab.write('d2' * 16, T0 + 8.8, bits=NEWER, terminal=('undelivered', 'link_down'))
        res = "ok; not delivered to device 'coupler' (link down)"
        lab.polls_every(T0, T0 + 30, lambda t: ([entry(l1)] if t >= T0 + 2 else [])
                        + ([entry(l2, bits=NEWER, applied=NEWER, result=res)] if t >= T0 + 9 else []))
        lab.answered_polls(T0 + 16.4, T0 + 18)
        lab.device(T0 + 16.4, 'sp_flow', NEWER)

    def test_an_explicit_link_down_interval_explains_it_even_in_an_unsequenced_log(self):
        lab = Lab(self)
        lab.sequenced = False                                         # the soak logs of 2026-10-29 carry no `seq`
        self.replaced_after_a_quiet_stretch(lab)
        lab.mode(T0 + 1.0, 'refuse')
        lab.mode(T0 + 12.0, 'normal')
        rep = lab.run()
        self.assertEqual(device_row(rep, 'd1' * 16).get('device'), 'link_down_until_replaced', rep['unexplained'])
        self.assertEqual(rep['verdict'], 'CLEAN')
        lab.slave[-1]['t'] = T0 + 5.0                                 # the link came back before the newer write: 12.5 should have gone out
        lab.answered_polls(T0 + 5.0, T0 + 6.0)
        self.assertEqual(lab.run()['verdict'], 'DISCREPANCY')

    def test_silence_in_an_unsequenced_log_without_a_mode_change_proves_nothing(self):
        lab = Lab(self)
        lab.sequenced = False
        self.replaced_after_a_quiet_stretch(lab)
        rep = lab.run()
        self.assertEqual(cls(rep, 'd1' * 16), 'uncheckable_device_log')
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_silence_in_a_sequenced_log_that_covers_the_stretch_is_no_link(self):
        lab = Lab(self)
        self.replaced_after_a_quiet_stretch(lab)
        rep = lab.run()
        self.assertEqual(device_row(rep, 'd1' * 16).get('device'), 'link_down_until_replaced')
        self.assertEqual(rep['verdict'], 'CLEAN')

    def test_a_log_that_ends_before_the_write_cannot_vouch_for_it(self):
        lab = Lab(self)
        lab.answered_polls(T0 - 0.5, T0 - 0.1)
        lab.log_end = None                                            # no `stopped`: the log just ends at T0 - 0.1
        label = lab.write('d3' * 16, T0 + 1)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'd3' * 16), 'uncheckable_device_log')
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_log_that_ends_inside_the_stretch_cannot_vouch_for_it(self):
        lab = Lab(self)
        self.replaced_after_a_quiet_stretch(lab)
        lab.slave.append({'ev': 'accept', 'conn': 9, 'mode': 'normal', 't': T0 + 3.0})
        lab.slave = [r for r in lab.slave if r['t'] < T0 + 4]
        lab.log_end = None                                            # cut off at T0 + 3, before the newer write
        rep = lab.run()
        self.assertEqual(cls(rep, 'd1' * 16), 'uncheckable_device_log')
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_device_stopped_and_started_again_leaves_a_gap(self):
        lab = Lab(self)
        self.replaced_after_a_quiet_stretch(lab)
        lab.slave += [{'ev': 'stopped', 't': T0 + 1.1}, {'ev': 'listen', 't': T0 + 16.3, 'port': 13511}]
        rep = lab.run()
        self.assertEqual(cls(rep, 'd1' * 16), 'uncheckable_device_log')
        self.assertEqual(rep['device_logs']['1']['gaps'], [[T0 + 1.1, T0 + 16.3]])

    def test_a_missing_record_in_a_sequenced_log_leaves_a_gap(self):
        lab = Lab(self)
        self.replaced_after_a_quiet_stretch(lab)
        lab.slave.append({'ev': 'accept', 'conn': 9, 'mode': 'normal', 't': T0 + 3.0})
        rows = lab.device_log()
        del rows[next(i for i, r in enumerate(rows) if r['t'] == T0 + 3.0)]    # `seq` jumps over it
        lab.files()
        (lab.dir / 'slave-1.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in rows))
        rep = reconcile_or_fail(self, lab.dir)
        self.assertEqual(cls(rep, 'd1' * 16), 'uncheckable_device_log')
        self.assertTrue(rep['device_logs']['1']['gaps'])

    def test_a_gap_leaves_it_open_even_when_the_device_answered_around_it(self):
        """The value may have arrived late, inside the gap: neither delivered nor missing can be shown."""
        lab = Lab(self)
        label = lab.write('d8' * 16, T0 + 1)
        lab.answered_polls(T0, T0 + 3)
        lab.slave += [{'ev': 'stopped', 't': T0 + 3.05}, {'ev': 'listen', 't': T0 + 3.5, 'port': 13511}]
        lab.answered_polls(T0 + 3.6, T0 + 30)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'd8' * 16), 'uncheckable_device_log')
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_no_effect_needs_a_log_that_was_there_to_see_one(self):
        lab = Lab(self)
        lab.answered_polls(T0 - 0.5, T0 - 0.1)
        lab.log_end = None
        lab.write('d9' * 16, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.polls_every(T0, T0 + 30, lambda t: [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'd9' * 16), 'uncheckable_device_log')
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_one_answered_request_in_the_stretch_rules_out_no_link(self):
        lab = Lab(self)
        self.replaced_after_a_quiet_stretch(lab)
        lab.answered_polls(T0 + 5.0, T0 + 5.0)
        rep = lab.run()
        self.assertEqual(cls(rep, 'd1' * 16), 'unexplained')

    def test_a_link_down_until_a_complete_log_ends(self):
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 0.9)
        lab.mode(T0 + 1.0, 'close')
        label = lab.write('d4' * 16, T0 + 1.2)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(device_row(rep, 'd4' * 16).get('device'), 'link_down_until_log_end')
        self.assertEqual(rep['verdict'], 'CLEAN')

    def test_a_link_down_until_a_cut_off_log_ends_proves_nothing_after_it(self):
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 0.9)
        label = lab.write('da' * 16, T0 + 1.2)
        lab.mode(T0 + 5.0, 'normal')                                  # the last record: no `stopped`, the log is cut here
        lab.log_end = None
        lab.polls_every(T0, T0 + 30, lambda t: [entry(label)] if t >= T0 + 2 else [])
        rep = lab.run()
        self.assertEqual(cls(rep, 'da' * 16), 'uncheckable_device_log', device_row(rep, 'da' * 16))
        lab.log_end = T0 + 5.1
        self.assertEqual(device_row(lab.run(), 'da' * 16).get('device'), 'link_down_until_log_end')

    def test_unusable_device_logs_are_refused(self):
        def no_start(rows):
            return rows[1:]

        def mixed_seq(rows):
            return [rows[0]] + [{k: v for k, v in r.items() if k != 'seq'} for r in rows[1:]]

        def damaged(rows):
            return rows + ['{"ev": "req", "t": 18000']

        def endless(rows):
            return rows[:-1] + ['{"ev": "stopped", "t": Infinity, "seq": %d}' % rows[-1]['seq']]

        for name, spoil in (('empty file', lambda rows: []), ('no start record', no_start), ('mixed seq', mixed_seq),
                            ('damaged record', damaged), ('a time that is not finite', endless)):
            with self.subTest(name):
                lab = Lab(self)
                label = lab.write('d5' * 16, T0 + 1)
                lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)
                lab.polls_every(T0, T0 + 20, lambda t: [entry(label)] if t >= T0 + 2 else [])
                d = lab.files()
                rows = spoil(lab.device_log())
                (d / 'slave-1.jsonl').write_text(''.join((r if isinstance(r, str) else json.dumps(r)) + '\n' for r in rows))
                assert_refused(self, d)
        lab = Lab(self)
        lab.polls_every(T0, T0 + 5, lambda t: [])
        lab.device_map = dict(DEVICE_MAP, d2_sp={'device': '2', 'kind': 'f32', 'addr': 100})
        d = lab.files()
        self.assertIn('role', assert_refused(self, d))                # a mapped device with no log

    def test_an_archive_with_no_usable_poll_is_refused(self):
        lab = Lab(self)
        lab.write('d6' * 16, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lab.poll(T0, [], rid=RID, rid_after=RID2)
        d = lab.files()
        assert_refused(self, d)

    def test_command_line_reports_an_evidence_gap_with_exit_2_and_a_report(self):
        lab = Lab(self)
        label = lab.write('d7' * 16, T0 + 1)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(label)] if t >= T0 + 2 else [])
        lab.log_end = None
        lab.answered_polls(T0 - 0.5, T0 - 0.1)
        d = lab.files()
        args = [sys.executable, '-I', '-B', str(ROOT / 'tools' / 'reconcile_report.py'), '--journal', str(d / 'journal.jsonl'),
                '--audit-archive', str(d / 'audit.jsonl'), '--device-map', str(d / 'map.json'), '--device-log', '1=%s' % (d / 'slave-1.jsonl'),
                '--out', str(d / 'report.json')]
        r = subprocess.run(args, capture_output=True, text=True)
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertEqual(json.loads(r.stdout)['verdict'], 'INCOMPLETE')
        self.assertEqual(json.loads((d / 'report.json').read_text())['counts'].get('uncheckable_device_log'), 1)
        lab.device(T0 + 1.05, 'sp_flow', BITS_12_5)                   # positive evidence needs no coverage claim
        lab.files()
        self.assertEqual(subprocess.run(args, capture_output=True).returncode, 0)


def cli(d, lifecycle=False):
    """(exit code, report) of the command line on the files in `d`."""
    out = d / 'report.json'
    if out.exists():
        out.unlink()
    args = [sys.executable, '-I', '-B', str(ROOT / 'tools' / 'reconcile_report.py'), '--journal', str(d / 'journal.jsonl'),
            '--audit-archive', str(d / 'audit.jsonl'), '--device-map', str(d / 'map.json'), '--device-log', '1=%s' % (d / 'slave-1.jsonl'),
            '--out', str(out)] + (['--lifecycle', str(d / 'life.jsonl')] if lifecycle else [])
    r = subprocess.run(args, capture_output=True, text=True, timeout=60)
    return r.returncode, json.loads(out.read_text()) if out.exists() else None


class CompleteLogBeforeTheWrite(unittest.TestCase):
    """Re-review R1: a complete device log (`listen` ... `stopped`) that ends before a write vouches for nothing after its end --
    neither with an unbroken `seq` nor, in the older format, with an explicit link-down `mode` that runs to its end."""

    def lab(self, sequenced):
        lab = Lab(self)
        lab.sequenced = sequenced
        lab.log_end = T0 + 0.5                                        # `stopped` half a second before the write
        if sequenced:
            lab.answered_polls(T0, T0)
        else:
            lab.mode(T0, 'refuse')
        label = lab.write('e1' * 16, T0 + 1)
        lab.polls_every(T0, T0 + 30, lambda t: [entry(label)] if t >= T0 + 2 else [])
        return lab

    def test_on_the_command_line(self):
        for sequenced in (True, False):
            with self.subTest('seq' if sequenced else 'mode records, no seq'):
                lab = self.lab(sequenced)
                code, rep = cli(lab.files())
                self.assertEqual(code, 2, rep and rep['actions'])
                self.assertEqual(rep['verdict'], 'INCOMPLETE')
                self.assertEqual(device_row(rep, 'e1' * 16).get('device'), 'uncheckable_device_log')
                lab.log_end = T0 + 30                                 # the same log running on past the write: down to its end
                code, rep = cli(lab.files())
                self.assertEqual(code, 0, rep and rep['actions'])
                self.assertEqual(device_row(rep, 'e1' * 16).get('device'), 'link_down_until_log_end')

    def test_coverage_is_an_ordered_finite_stretch_inside_the_log(self):
        lab = Lab(self)                                               # listen at T0 - 1, stopped at T0 + 60, nothing answered
        log = rr.DeviceLog('1', str(lab.files() / 'slave-1.jsonl'))
        self.assertTrue(log.covers(T0, T0 + 1))
        for t0, t1 in ((T0 + 1, T0), (T0, math.inf), (-math.inf, T0), (math.nan, T0), (T0 + 59, T0 + 61)):
            self.assertFalse(log.covers(t0, t1), (t0, t1))
        self.assertTrue(log.no_link(T0, T0 + 1))
        self.assertFalse(log.no_link(T0 + 1, T0 + 1))                 # a moment is not a stretch without a link
        self.assertFalse(log.no_link(T0 + 1, T0))


class ApplyOrder(unittest.TestCase):
    """Re-review R2: which of two writes the runtime applied first is known from the bridge's answers and the poll times (one
    interval ends before the other begins), or from a value reaching the device before the other write was sent -- never from
    the ring -- and the device check uses the same order.  A result that depends on an order nothing fixes is an evidence gap."""

    A, B = BITS_12_5, NEWER

    def a_times_out_then_b(self, lab, a_first_seen=T0 + 6):
        """12.5 sent at T0 + 1 and never answered (its entry first seen at `a_first_seen`); 15.5 sent at T0 + 4.5, answered."""
        la = lab.write('aa' * 16, T0 + 1, status=None, error='timeout', terminal=('unknown', 'runtime_unreachable'))
        lb = lab.write('bb' * 16, T0 + 4.5, bits=self.B, status=None, terminal=None)
        lab.rec('response', T0 + 4.51, action_id='bb' * 16, http={'kind': 'response', 'status': 200})
        lab.poll(T0, [])
        lab.poll(a_first_seen, [entry(la)] + ([entry(lb, bits=self.B, applied=self.B)] if a_first_seen > T0 + 4.51 else []))
        lab.polls_every(T0 + 7, T0 + 30, lambda t: [entry(la), entry(lb, bits=self.B, applied=self.B)])
        lab.answered_polls(T0, T0 + 30)

    def device_check(self, writes, **kw):
        lab = Lab(self)
        self.a_times_out_then_b(lab, **kw)
        for t, bits in writes:
            lab.device(t, 'sp_flow', bits)
        return lab.run()

    def test_device_sequences_that_only_one_of_two_possible_orders_explains(self):
        A, B = self.A, self.B
        for writes, decided in (([(T0 + 4.7, A), (T0 + 5.0, B)], (T0 + 5.0, B)),        # explained if 12.5 came first
                                ([(T0 + 4.55, B), (T0 + 4.7, A)], (T0 + 4.7, A))):      # explained if 15.5 came first
            with self.subTest(writes):
                rep = self.device_check(writes)
                self.assertEqual(rep['unexplained_device_writes'], [])
                self.assertEqual([(d['t'], d['bits']) for d in rep['unchecked_device_writes']], [decided])
                self.assertEqual(rep['unchecked_device_writes'][0]['order_decided_by'], ['aa' * 16, 'bb' * 16])
                self.assertEqual([cls(rep, a) for a in ('aa' * 16, 'bb' * 16)], ['consistent', 'consistent'])
                self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_device_sequence_no_order_explains(self):
        A, B = self.A, self.B
        rep = self.device_check([(T0 + 4.7, A), (T0 + 5.0, B), (T0 + 5.5, A)])
        self.assertEqual([(d['t'], d['bits']) for d in rep['unexplained_device_writes']], [(T0 + 5.5, A)])
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_device_sequences_in_a_known_order(self):
        A, B = self.A, self.B                                         # 12.5's entry was read before 15.5 was sent
        rep = self.device_check([(T0 + 4.2, A), (T0 + 4.55, B)], a_first_seen=T0 + 4.4)
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained_device_writes'], rep['unchecked_device_writes']))
        rep = self.device_check([(T0 + 4.55, B), (T0 + 4.7, A)], a_first_seen=T0 + 4.4)
        self.assertEqual([(d['t'], d['bits']) for d in rep['unexplained_device_writes']], [(T0 + 4.7, A)])   # older after newer
        self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_a_value_on_the_device_before_the_other_write_was_sent_fixes_the_order(self):
        """The 60-minute soak (d2_sp, 2026-10-29): 27.875 timed out, the device got it 24 ms before 7.492 was sent; nothing
        else could have put 27.875 into the VM, so it was applied first."""
        A, B = self.A, self.B
        rep = self.device_check([(T0 + 4.4, A), (T0 + 4.55, B)])
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained_device_writes'], rep['unchecked_device_writes']))
        rep = self.device_check([(T0 + 4.6, A), (T0 + 4.65, B)])     # got it after 15.5 was sent: no proof which came first
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_value_two_writes_could_have_put_there_proves_no_order(self):
        """12.5 was applied once already at T0 + 0.5, so 12.5 on the device at T0 + 4.4 does not show that the second 12.5 was
        applied by then.  12.5 again after 15.5 arrived is a stale write if the second 12.5 came first, explained if it came last."""
        A, B = self.A, self.B
        lab = Lab(self)
        l0 = lab.write('a0' * 16, T0 + 0.5)
        self.a_times_out_then_b(lab)
        lab.polls = [dict(p, audit=[entry(l0)] + p['audit']) if 'audit' in p and p['t'] >= T0 + 0.6 else p for p in lab.polls]
        for t, bits in ((T0 + 0.55, A), (T0 + 4.4, A), (T0 + 4.55, B), (T0 + 4.7, A)):
            lab.device(t, 'sp_flow', bits)
        rep = lab.run()
        self.assertEqual(rep['unexplained_device_writes'], [])
        self.assertEqual([(d['t'], d['bits']) for d in rep['unchecked_device_writes']], [(T0 + 4.7, A)])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_every_value_before_it_in_an_order_must_have_been_applicable_by_then(self):
        """Called directly: a device write at T0 + 4.3 of the value applied in [T0 + 1, T0 + 6] fits the order in which it came
        first, not the one in which it followed a value sent at T0 + 4.5."""
        a = {'bits': 7, 'lo': (T0 + 1, 1), 'hi': (T0 + 6, math.inf), 'action_id': 'a'}
        b = {'bits': 9, 'lo': (T0 + 4.5, 4), 'hi': (T0 + 4.51, 5), 'action_id': 'b'}
        w = {'t': T0 + 4.3, 'bits': 7}
        self.assertEqual(rr.check_device_sequence([a, b], [w]), ([], [(w, ['a', 'b'])]))
        self.assertEqual(rr.check_device_sequence([a, dict(b, lo=(T0 + 4.2, 4))], [w]), ([], []))

    def test_concurrent_writes_whose_audit_order_is_not_the_apply_order(self):
        """The runtime measurement of the re-review: concurrent ordinary writes, every answer 200, the last two /audit entries in
        the opposite order to the VM.  Four writes sent within 3 ms, all answered 10 ms later; the device got only the last
        value the VM took.  Nothing says which came last: no discrepancy, and no clean result either."""
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        values = [1092616192 + i for i in range(4)]
        aids = ['c%d' % i * 16 for i in range(4)]
        for i, (aid, v) in enumerate(zip(aids, values)):
            lab.rec('intent', T0 + 1 + i * 0.001, action_id=aid, label='tier0.lab.%s' % aid[:8], variable='sp_flow', bits=v,
                    runtime_id=RID, target_key='sp_flow')
        for i, aid in enumerate(aids):
            lab.rec('response', T0 + 1.010 + i * 0.001, action_id=aid, http={'kind': 'response', 'status': 200})
        ring = [entry('tier0.lab.%s' % aids[i][:8], bits=values[i], applied=values[i]) for i in (0, 1, 3, 2)]
        lab.polls_every(T0, T0 + 30, lambda t: ring if t >= T0 + 2 else [])
        lab.device(T0 + 1.05, 'sp_flow', values[3])
        rep = lab.run()
        self.assertEqual(rep['unexplained'], [])
        self.assertEqual(rep['unexplained_device_writes'], [])
        self.assertEqual([cls(rep, a) for a in aids], ['uncheckable_order'] * 3 + ['consistent'])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')


class JointOrder(unittest.TestCase):
    """Independent review r2 (2026-10-29): every full combination of orders, and every way an unrecorded handover could have
    split the device writes, must explain the device -- not each group or each write taken alone."""

    def lamp(self, lab, aid, t, bits, timeout=False, rid=RID):
        if timeout:
            return lab.write(aid, t, var='lamp', bits=bits, rid=rid, status=None, error='timeout',
                             terminal=('unknown', 'runtime_unreachable'))
        return lab.write(aid, t, var='lamp', bits=bits, rid=rid)

    def two_pairs(self, known=None):
        """lamp 0 (no answer) at T0 + 1 and 1 at T0 + 4.5 in an unknown order; again 0 at T0 + 7 and 1 at T0 + 10.5.  The first
        pair surely came before the second.  `known` gives each write an answer 0.1 s after it instead, fixing the order."""
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        plan = known or (('a', T0 + 1, 0, True), ('b', T0 + 4.5, 1, False), ('c', T0 + 7, 0, True), ('d', T0 + 10.5, 1, False))
        labels = {}
        for name, t, bits, timeout in plan:
            labels[name] = (self.lamp(lab, name * 32, t, bits, timeout), bits)
        ent = lambda names: [entry(labels[n][0], var='lamp', bits=labels[n][1], applied=labels[n][1]) for n in names]
        lab.poll(T0, [])
        lab.poll(T0 + 6, ent('ab'))
        lab.poll(T0 + 12, ent('abcd'))
        lab.poll(T0 + 30, ent('abcd'))
        for t, bits in ((T0 + 1.5, 0), (T0 + 4.7, 1), (T0 + 10.7, 0), (T0 + 10.8, 1)):
            lab.device(t, 'lamp', bits)
        return lab.run()

    def test_every_full_combination_of_group_orders_must_explain_the_device(self):
        """0, 1, 0, 1 on the device: a-b-c-d, b-a-c-d and b-a-d-c explain it; a-b-d-c does not (1 after the last 0).  Each pair
        alone has an order that explains it, whatever the other pair's order is."""
        rep = self.two_pairs()
        self.assertEqual(rep['unexplained_device_writes'], [])
        self.assertEqual([(d['t'], d['bits']) for d in rep['unchecked_device_writes']], [(T0 + 10.8, 1)])
        self.assertEqual(rep['unchecked_device_writes'][0]['order_decided_by'], [c * 32 for c in 'abcd'])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_known_full_order_decides_it_either_way(self):
        rep = self.two_pairs(known=(('a', T0 + 1, 0, False), ('b', T0 + 4.5, 1, False), ('c', T0 + 7, 0, False),
                                    ('d', T0 + 10.6, 1, False)))
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained_device_writes'], rep['unchecked_device_writes']))
        rep = self.two_pairs(known=(('a', T0 + 1, 0, False), ('b', T0 + 4.5, 1, False), ('d', T0 + 7, 1, False),
                                    ('c', T0 + 10.6, 0, False)))
        self.assertEqual([(d['t'], d['bits']) for d in rep['unexplained_device_writes']], [(T0 + 10.8, 1)])

    def handover(self, got, pair=True):
        """No lifecycle log.  The old instance applied lamp 0 (no answer, entry read at T0 + 6) and 1 at T0 + 4.5 in an unknown
        order -- or, with pair=False, only 1 at T0 + 1; the archive last read it at T0 + 8 (T0 + 4) and first read the new
        instance, which applied nothing, at T0 + 12 (T0 + 8).  `got`: what the device got and when."""
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        if pair:
            la, lb = self.lamp(lab, 'aa' * 16, T0 + 1, 0, True), self.lamp(lab, 'bb' * 16, T0 + 4.5, 1)
            both = [entry(la, var='lamp', bits=0, applied=0), entry(lb, var='lamp', bits=1, applied=1)]
            lab.poll(T0, [])
            lab.poll(T0 + 6, both)
            lab.poll(T0 + 8, both)
            lab.poll(T0 + 12, [], rid=RID2)
        else:
            la = self.lamp(lab, 'aa' * 16, T0 + 1, 1)
            lab.poll(T0, [])
            lab.poll(T0 + 2, [entry(la, var='lamp', bits=1, applied=1)])
            lab.poll(T0 + 4, [entry(la, var='lamp', bits=1, applied=1)])
            lab.poll(T0 + 8, [], rid=RID2)
        lab.poll(T0 + 30, [], rid=RID2)
        for t, bits in got:
            lab.device(t, 'lamp', bits)
        return lab.run()

    def test_a_handover_keeps_the_order_the_old_instance_leaves_open(self):
        """0 at T0 + 9, during the handover: the new instance's initial output, or the old instance's 0 if it was applied
        after 1.  Neither is shown -- the order gap must not be lost because some owner explains it."""
        rep = self.handover([(T0 + 1.5, 0), (T0 + 4.7, 1), (T0 + 9, 0)])
        self.assertEqual(rep['unexplained_device_writes'], [])
        self.assertIn((T0 + 9, 0, [RID, RID2]),
                      [(d['t'], d['bits'], d.get('possible_instances')) for d in rep['unchecked_device_writes']])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')
        rep = self.handover([(T0 + 1.5, 0), (T0 + 4.7, 1), (T0 + 7, 0)])   # surely the old instance's: the same gap
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_one_handover_must_explain_every_write_made_during_it(self):
        """The old instance delivered 1; during the handover the device got 0 (only the new instance's initial output explains
        it) and then 1 (only the old instance's): that needs old, new, old -- no single handover."""
        rep = self.handover([(T0 + 1.1, 1), (T0 + 5, 0), (T0 + 6, 1)], pair=False)
        self.assertEqual([(d['t'], d['bits'], d.get('possible_instances')) for d in rep['unexplained_device_writes']],
                         [(T0 + 5, 0, [RID, RID2]), (T0 + 6, 1, [RID, RID2])])
        self.assertEqual(rep['verdict'], 'DISCREPANCY')
        rep = self.handover([(T0 + 1.1, 1), (T0 + 5, 1), (T0 + 6, 0)], pair=False)   # old, then new: a handover in between
        self.assertEqual(rep['unexplained_device_writes'], [])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_branch_keeps_the_orders_it_fixed_ahead_of_its_states(self):
        """Called directly; found by brute force over full orders (evidence/rereview2-20261029/oracle_crosscheck.py): every
        order of these five values explains 0 then 1 on the device.  A search that forgot, after each write, the orders a
        combination had fixed for groups its states had not reached would pair a state with an order it did not come from
        and report a gap that is not there."""
        ev = [{'action_id': aid, 'bits': bits, 'lo': (lo, n), 'hi': (hi, n)} for n, (aid, bits, lo, hi) in enumerate(
            (('e1', 1, 1.29, 1.30), ('e2', 0, 1.37, 2.37), ('e3', 0, 1.54, 4.04), ('e4', 0, 4.10, 6.60), ('e5', 1, 4.72, 5.02)), 1)]
        self.assertEqual(rr.check_device_sequence(ev, [{'t': 5.33, 'bits': 0}, {'t': 6.08, 'bits': 1}]), ([], []))

    def test_a_handover_past_its_budget_leaves_its_writes_unchecked(self):
        cap = rr.HANDOVER_CAP
        self.addCleanup(setattr, rr, 'HANDOVER_CAP', cap)
        rr.HANDOVER_CAP = 2                                           # two writes, two instances: three splits
        rep = self.handover([(T0 + 1.1, 1), (T0 + 5, 0), (T0 + 6, 1)], pair=False)
        self.assertEqual(rep['unexplained_device_writes'], [])
        self.assertEqual([(d['t'], d['bits']) for d in rep['unchecked_device_writes']], [(T0 + 5, 0), (T0 + 6, 1)])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')

    def test_a_search_past_its_budget_leaves_the_writes_unchecked(self):
        """Two values of 1 in an unknown order: every order explains the device, unless the search may not follow both."""
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        la = self.lamp(lab, 'aa' * 16, T0 + 1, 1, True)
        lb = self.lamp(lab, 'bb' * 16, T0 + 4.5, 1)
        lab.poll(T0, [])
        lab.polls_every(T0 + 6, T0 + 30, lambda t: [entry(la, var='lamp', bits=1, applied=1), entry(lb, var='lamp', bits=1, applied=1)])
        lab.device(T0 + 4.7, 'lamp', 1)
        self.assertEqual(lab.run()['verdict'], 'CLEAN')
        cap = rr.BRANCH_CAP
        self.addCleanup(setattr, rr, 'BRANCH_CAP', cap)
        rr.BRANCH_CAP = 1
        rep = lab.run()
        self.assertEqual([(d['t'], d['bits']) for d in rep['unchecked_device_writes']], [(T0 + 4.7, 1)])
        self.assertEqual(rep['verdict'], 'INCOMPLETE')


class InstanceBoundary(unittest.TestCase):
    """Re-review R3: a value an instance applied must reach the device while that instance is live.  The next instance writing
    the same value delivers nothing for it, and the end of an instance replaces a value only within one output cycle."""

    def restart(self, spawned, again):
        """12.5 applied at T0 + 1 in RID, the device answering throughout and never getting it from RID; RID2 spawned at
        `spawned` writes its initial 0, then 12.5 again at `again`, which reaches the device."""
        lab = Lab(self)
        lab.answered_polls(T0, T0 + 30)
        l1 = lab.write('f1' * 16, T0 + 1)
        l2 = lab.write('f2' * 16, again, rid=RID2)
        lab.life += [{'ev': 'runtime_ready', 't': T0 - 1, 'spawned_t': T0 - 2, 'runtime_id': RID},
                     {'ev': 'runtime_ready', 't': spawned + 0.5, 'spawned_t': spawned, 'runtime_id': RID2}]
        lab.device(spawned + 0.1, 'sp_flow', 0)
        lab.device(again + 0.05, 'sp_flow', BITS_12_5)
        lab.polls_every(T0, spawned - 0.5, lambda t: [entry(l1)] if t >= T0 + 2 else [])
        lab.polls_every(spawned + 0.5, T0 + 30, lambda t: [entry(l2)] if t >= again + 0.5 else [], rid=RID2)
        return lab.run(lifecycle=True)

    def test_the_next_instance_delivering_the_same_value_delivers_nothing_for_this_one(self):
        for again in (T0 + 7, T0 + 8):
            with self.subTest(again - T0):
                rep = self.restart(T0 + 5.5, again)
                self.assertEqual(cls(rep, 'f1' * 16), 'unexplained', device_row(rep, 'f1' * 16))
                self.assertEqual(device_row(rep, 'f2' * 16).get('device'), 'seen')
                self.assertEqual(rep['verdict'], 'DISCREPANCY')

    def test_a_restart_replaces_a_value_only_within_one_output_cycle(self):
        rep = self.restart(T0 + 1 + 1.9, T0 + 3.0)
        self.assertEqual(device_row(rep, 'f1' * 16).get('device'), 'replaced_before_delivery', rep['unexplained'])
        self.assertEqual(device_row(rep, 'f1' * 16).get('replaced_by'), 'restart')
        self.assertEqual(rep['verdict'], 'CLEAN', (rep['unexplained'], rep['unexplained_device_writes']))
        rep = self.restart(T0 + 1 + 2.1, T0 + 3.2)
        self.assertEqual(cls(rep, 'f1' * 16), 'unexplained', device_row(rep, 'f1' * 16))

    def test_without_a_lifecycle_record_a_delivery_during_the_handover_is_undecided(self):
        """No lifecycle log: the archive last read RID at T0 + 4 and first read RID2 at T0 + 7; either may have been live in
        between."""
        for got, expect in ((T0 + 3.5, 'consistent'), (T0 + 5.0, 'uncheckable_order'), (T0 + 7.5, 'unexplained')):
            with self.subTest(got - T0):
                lab = Lab(self)
                lab.answered_polls(T0, T0 + 30)
                l1 = lab.write('f3' * 16, T0 + 1)
                lab.device(got, 'sp_flow', BITS_12_5)
                lab.polls_every(T0, T0 + 4, lambda t: [entry(l1)] if t >= T0 + 2 else [])
                lab.polls_every(T0 + 7, T0 + 30, lambda t: [], rid=RID2)
                rep = lab.run()
                self.assertEqual(cls(rep, 'f3' * 16), expect, device_row(rep, 'f3' * 16))


if __name__ == '__main__':
    unittest.main()
