"""One test (or more) per weakness W1-W19 of HANDOFF-2026-10-28 section 5, as fixed.  No ports, no runtime."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path, decode_event
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import robustness_cases as rc  # noqa: E402
from bridge import auth, durable, governance_check, preflight, registry  # noqa: E402
from bridge.errors import Reject  # noqa: E402
from bridge.engine import Engine, make_label  # noqa: E402
from bridge.journal import Journal  # noqa: E402
from bridge.tail import RequestTail  # noqa: E402
from signing_fixtures import TraceWriter  # noqa: E402

ROOT = rc.ROOT
KEYS = {'k1': {'secret': b'\x07' * 32, 'not_before_ms': 0, 'not_after_ms': 2 ** 60}}
REG_DOC = {'version': 1, 'targets': [{'key': 'flow', 'device_id': 'bench01-sim', 'ia2_variable': 'sp_flow', 'type': 'REAL', 'min': 0.0, 'max': 40.0,
                                      'simulation': True, 'enabled': True, 'require_devices': ['coupler'], 'min_interval_ms': 0, 'max_rate_per_min': 60,
                                      'verify': {'feedback': 'sp_flow_fb', 'tolerance': 0.05, 'settle_ms': 5000}}]}


def lines(path):
    with open(path, encoding='utf-8') as f:
        return f.readlines()


def read(path):
    with open(path, encoding='utf-8') as f:
        return f.read()


def rb(path):
    with open(path, 'rb') as f:
        return f.read()


def write(path, text):
    with open(path, 'w', encoding='utf-8') as f:
        f.write(text)


def make(tmp, client=None, cfg=None, t0=1_700_000_000_000, journal_clock=None, registry_doc=None):
    t = [t0]

    def clock():
        t[0] += 1
        return t[0]
    jr = Journal(os.path.join(tmp, 'j.jsonl'), journal_clock or clock)
    jr.open()
    tw = TraceWriter(os.path.join(tmp, 't.jsonl'))
    reg = registry.parse(json.dumps(registry_doc or REG_DOC), 'sha-of-the-registry')
    conf = {'poll_ms': 1, 'advance_wait_ms': 50, 'hold_path': None, 'project_dir': None, 'reconcile_recheck_ms': 10000}
    conf.update(cfg or {})
    eng = Engine(conf, reg, KEYS, jr, client or rc.FakeClient(), tw, clock=clock, sleep=lambda s: None)
    return eng, jr, tw, t


def line(t, **over):
    p = rc.payload(requested_at_ms=t[0], deadline_ms=t[0] + 30000, **over)
    text = json.dumps(p)
    return json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': text, 'sig': auth.sign(KEYS['k1']['secret'], text)}), p


def close_all(*things):
    for x in things:
        x.close()


class Timeout(rc.FakeClient):
    """Every POST /write times out (the write may or may not land)."""

    def request(self, method, path, body=None, origin=None):
        return {'kind': 'error', 'error_kind': 'timeout', 'error_text': '', 't_send_ms': 0, 't_recv_ms': 0, 'method': method, 'path': path,
                'req_body': json.dumps(body), 'origin': origin}


class W1_EventCarriesWhatTheReadSideNeeds(unittest.TestCase):
    def test_the_accepted_event_has_the_values_and_the_scan_clock_markers(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            ln, p = line(t)
            eng.handle_line(ln)
            ev = jr.final_event(p['action_id'])
            self.assertEqual(ev['status'], 'accepted')
            for k in ('requested_value', 'deadline_ms', 'requested_at_ms', 'device_id', 'session_id', 'target_key', 'runtime_id', 'sent_at_ms'):
                self.assertIsNotNone(ev.get(k), k)
            self.assertEqual(ev['registry_sha256'], 'sha-of-the-registry')
            self.assertIsInstance(ev['post_scan_us'], int)
            self.assertIsInstance(ev['scan_anchor_ms'], int)
            close_all(jr, tw)


class W2_Anchor(unittest.TestCase):
    def test_a_journal_cut_short_is_noticed_through_the_trace(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            ln, p = line(t)
            eng.handle_line(ln)
            seq = jr.seq
            close_all(jr, tw)
            tw2 = TraceWriter(os.path.join(tmp, 't.jsonl'))
            self.assertEqual(tw2.last_anchor[0], seq)
            jr2 = Journal(os.path.join(tmp, 'j.jsonl'), lambda: 1)
            jr2.open()
            self.assertIsNone(jr2.check_anchor(tw2.last_anchor), 'untouched: fine')
            jr2.close()
            path = os.path.join(tmp, 'j.jsonl')
            lines = rb(path).split(b'\n')[:-1]
            with open(path, 'wb') as f:
                f.write(b'\n'.join(lines[:-3]) + b'\n')               # the action's last records are gone; the chain is still valid
            jr3 = Journal(path, lambda: 1)
            jr3.open()
            self.assertIn('cut off its tail', jr3.check_anchor(tw2.last_anchor))
            jr3.close()
            tw2.close()

    def test_a_rewritten_tail_is_noticed_too(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            ln, p = line(t)
            eng.handle_line(ln)
            anchor_seq = jr.seq
            close_all(jr, tw)
            tw2 = TraceWriter(os.path.join(tmp, 't.jsonl'))
            path = os.path.join(tmp, 'j.jsonl')
            lines = rb(path).split(b'\n')[:-1]
            with open(path, 'wb') as f:
                f.write(b'\n'.join(lines[:-1]) + b'\n')
            jr2 = Journal(path, lambda: 1)
            jr2.open()
            while jr2.seq < anchor_seq + 2:                            # grow it past the anchor with different content
                jr2.append('start', note='something else')
            self.assertIn('rewritten', jr2.check_anchor(tw2.last_anchor))
            jr2.close()
            tw2.close()

    def test_every_trace_event_carries_the_head_it_was_written_after(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            ln, p = line(t)
            eng.handle_line(ln)
            events = [decode_event(json.loads(l)) for l in lines(os.path.join(tmp, 't.jsonl'))]
            self.assertTrue(events and all('journal_seq' in e and 'journal_hash' in e for e in events))
            self.assertEqual(events[-1]['journal_hash'], jr.last_hash)
            close_all(jr, tw)


class W3_Enable(unittest.TestCase):
    def test_an_operator_line_re_enables_a_disabled_target_once_and_survives_restarts(self):
        with tempfile.TemporaryDirectory() as tmp:
            enable = os.path.join(tmp, 'j.jsonl.enable')
            eng, jr, tw, t = make(tmp, cfg={'enable_path': enable})
            jr.append('disable', target_key='flow', reason='unknown_variable', action_id='x' * 32)
            self.assertIn('flow', jr.disabled)
            ln, p = line(t)
            eng.handle_line(ln)
            self.assertEqual(jr.final_event(p['action_id'])['reason'], 'not_in_registry', 'disabled: refused')
            write(enable, json.dumps({'target_key': 'flow', 'reason': 'variable name fixed in the project, reviewed by Mercy'}) + '\n'
                                    + 'not json\n' + json.dumps({'target_key': 'nope', 'reason': 'x'}) + '\n')
            eng.apply_enables()
            eng.apply_enables()                                           # applying twice changes nothing
            kinds = [json.loads(l)['kind'] for l in lines(os.path.join(tmp, 'j.jsonl'))]
            self.assertEqual((kinds.count('enable'), kinds.count('enable_ignored')), (1, 2))
            self.assertNotIn('flow', jr.disabled)
            ln2, p2 = line(t)
            eng.handle_line(ln2)
            self.assertEqual(jr.final_event(p2['action_id'])['status'], 'accepted')
            close_all(jr, tw)
            # restart: the enable is replayed in order; a LATER disable is not undone by the old enable line
            eng2, jr2, tw2, t2 = make(tmp, cfg={'enable_path': enable})
            self.assertNotIn('flow', jr2.disabled)
            jr2.append('disable', target_key='flow', reason='unknown_variable', action_id='y' * 32)
            eng2.apply_enables()
            self.assertIn('flow', jr2.disabled, 'the old enable line was already applied; it must not re-apply')
            close_all(jr2, tw2)


class W4_LabelCollision(unittest.TestCase):
    def test_a_second_action_with_the_same_label_is_refused_before_anything_is_sent(self):
        with tempfile.TemporaryDirectory() as tmp:
            client = rc.FakeClient()
            eng, jr, tw, t = make(tmp, client)
            ln1, p1 = line(t, action_id='deadbeef' + '0' * 24)
            ln2, p2 = line(t, action_id='deadbeef' + '1' * 24)
            self.assertEqual(make_label(p1), make_label(p2))
            eng.handle_line(ln1)
            posts_before = len([c for c in client.calls if c == '/write'])
            eng.handle_line(ln2)
            ev2 = jr.final_event(p2['action_id'])
            self.assertEqual((ev2['status'], ev2['reason']), ('rejected', 'conflicting_action_id'))
            self.assertIn('label', ev2['detail'])
            self.assertEqual(jr.actions[p1['action_id']]['terminal']['event']['status'], 'accepted')
            close_all(jr, tw)


class W6_ProductionPackage(unittest.TestCase):
    def test_the_production_build_has_no_hook_loading_at_all(self):
        with tempfile.TemporaryDirectory() as tmp:
            r = subprocess.run([sys.executable, '-I', os.path.join(ROOT, 'tools', 'package_prod.py'), os.path.join(tmp, 'pkg')], capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stderr)
            pkg = os.path.join(tmp, 'pkg')
            self.assertFalse(os.path.exists(os.path.join(pkg, 'bridge', 'testhooks_loader.py')))
            for base, _, files in os.walk(pkg):
                for name in files:
                    text = read(os.path.join(base, name))
                    for word in ('test-hooks', 'test_hooks', 'testhooks', 'importlib'):
                        self.assertNotIn(word, text, '%s mentions %s' % (name, word))
            helpp = subprocess.run([sys.executable, '-I', os.path.join(pkg, 'bridge_run.py'), '--help'], capture_output=True, text=True)
            self.assertEqual(helpp.returncode, 0)
            self.assertNotIn('hooks', helpp.stdout)
            args = ['--registry', 'r', '--registry-sha256', 'x', '--keys', 'k', '--event-keys', event_keys_path('k'), '--runtime', '127.0.0.1:1', '--journal', 'j', '--requests', 'q',
                    '--trace', 't', '--project-dir', 'p', '--test-hooks', 'evil.py']
            bad = subprocess.run([sys.executable, '-I', os.path.join(pkg, 'bridge_run.py')] + args, capture_output=True, text=True)
            self.assertEqual(bad.returncode, 2)
            self.assertIn('unrecognized arguments', bad.stderr)
            lab = subprocess.run([sys.executable, '-I', os.path.join(ROOT, 'bridge_run.py'), '--help'], capture_output=True, text=True)
            self.assertIn('--test-hooks', lab.stdout, 'the lab build still has it')


class W7_Keys(unittest.TestCase):
    def test_a_key_file_readable_by_others_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = os.path.join(tmp, 'keys.json')
            write(p, json.dumps({'k1': {'secret_hex': '07' * 32, 'not_after_ms': 2 ** 50}}))
            os.chmod(p, 0o644)
            with self.assertRaises(ValueError):
                auth.load_keys(p)
            os.chmod(p, 0o600)
            self.assertIn('k1', auth.load_keys(p))

    def test_rotation_two_keys_with_overlapping_windows(self):
        old = {'secret': b'\x01' * 32, 'not_before_ms': 0, 'not_after_ms': 1000}
        new = {'secret': b'\x02' * 32, 'not_before_ms': 900, 'not_after_ms': 5000}
        keys = {'old': old, 'new': new}

        def env(kid, secret):
            return json.dumps({'v': 'ia2.control/v0', 'key_id': kid, 'payload': '{}', 'sig': auth.sign(secret, '{}')})
        auth.verify_envelope(env('old', old['secret']), keys, 500)
        auth.verify_envelope(env('old', old['secret']), keys, 950)         # overlap: both work
        auth.verify_envelope(env('new', new['secret']), keys, 950)
        with self.assertRaises(Reject):
            auth.verify_envelope(env('old', old['secret']), keys, 1001)    # the old key has expired
        auth.verify_envelope(env('new', new['secret']), keys, 1001)
        with self.assertRaises(Reject):
            auth.verify_envelope(env('new', new['secret']), keys, 899)     # the new one is not valid yet


class W8_Tail(unittest.TestCase):
    def test_lines_partial_lines_and_oversize_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = os.path.join(tmp, 'q')
            tail = RequestTail(p, max_line=10)
            self.assertEqual(tail.poll(), [])
            with open(p, 'ab') as f:
                f.write(b'abc\n\nhal')
            self.assertEqual(tail.poll(), [('line', 'abc')])
            with open(p, 'ab') as f:
                f.write(b'f\n' + b'x' * 10 + b'\n' + b'y' * 25)
            self.assertEqual(tail.poll(), [('line', 'half'), ('line', 'x' * 10)])
            with open(p, 'ab') as f:
                f.write(b'y' * 5 + b'\nok\n')
            out = tail.poll()
            import hashlib
            self.assertEqual(out[0], ('oversize', 30, hashlib.sha256(b'y' * 30).hexdigest()))
            self.assertEqual(out[1], ('line', 'ok'))

    def test_an_oversize_line_is_journaled_once_even_across_a_restart(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            eng.note_oversize(70000, 'h' * 64)
            eng.note_oversize(70000, 'h' * 64)
            close_all(jr, tw)
            eng2, jr2, tw2, _ = make(tmp)
            eng2.note_oversize(70000, 'h' * 64)
            rejects = [json.loads(l) for l in lines(os.path.join(tmp, 'j.jsonl')) if '"gate_reject"' in l]
            self.assertEqual(len(rejects), 1)
            self.assertEqual(eng2.counters['oversize_lines'], 1)
            close_all(jr2, tw2)


class W9_ReconcileTwice(unittest.TestCase):
    def test_a_write_that_lands_after_the_first_look_is_caught_by_the_second(self):
        with tempfile.TemporaryDirectory() as tmp:
            client = Timeout()
            eng, jr, tw, t = make(tmp, client)
            ln, p = line(t)
            eng.handle_line(ln)
            ev = jr.final_event(p['action_id'])
            self.assertEqual((ev['status'], ev['reason']), ('unknown', 'runtime_unreachable'))
            t[0] += 5000
            eng.reconcile_unknowns()                                        # first look: nothing in /audit
            notes = [decode_event(json.loads(l)) for l in lines(os.path.join(tmp, 't.jsonl')) if '"reconcile_note"' in l]
            self.assertEqual([(n['pass_no'], n['audit_entry'], n['landed'], n['final']) for n in notes], [(1, 'absent', None, False)])
            eng.reconcile_unknowns()
            self.assertEqual(len([l for l in lines(os.path.join(tmp, 't.jsonl')) if '"reconcile_note"' in l]), 1, 'not again before the pause')
            label = jr.actions[p['action_id']]['intent']['label']
            client.audit = [{'origin': label, 'name': 'sp_flow', 'requested': 1095237632, 'applied': 1095237632, 'result': 'ok'}]
            t[0] += 11000
            eng.reconcile_unknowns()                                        # second look: it has landed since
            notes = [decode_event(json.loads(l)) for l in lines(os.path.join(tmp, 't.jsonl')) if '"reconcile_note"' in l]
            self.assertEqual([(n['pass_no'], n['audit_entry'], n['landed'], n['final']) for n in notes],
                             [(1, 'absent', None, False), (2, 'found', True, True)])
            self.assertIn('AFTER the first look', notes[1]['detail'])
            t[0] += 60000
            eng.reconcile_unknowns()
            self.assertEqual(len([l for l in lines(os.path.join(tmp, 't.jsonl')) if '"reconcile_note"' in l]), 2, 'two looks, no more')
            self.assertEqual(jr.final_event(p['action_id'])['status'], 'unknown', 'the status never upgrades')
            close_all(jr, tw)


class W10_QueueTimeIsNotStaleness(unittest.TestCase):
    def test_a_request_that_waited_in_the_bridges_own_queue_is_not_expired(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            ln, p = line(t)                                                  # requested_at = now
            arrived = t[0] + 100
            t[0] += 5000                                                    # then 5 s pass before the bridge gets to it
            eng.handle_line(ln, arrived_ms=arrived)
            self.assertEqual(jr.final_event(p['action_id'])['status'], 'accepted')
            ln2, p2 = line(t)
            t[0] += 5000
            eng.handle_line(ln2)                                            # no arrival time: judged at processing time, as before
            self.assertEqual(jr.final_event(p2['action_id'])['reason'], 'expired')
            close_all(jr, tw)

    def test_a_dead_runtime_costs_one_failed_read_not_three(self):
        class Dead(rc.FakeClient):
            def get_json(self, path):
                self.calls.append(path)
                return None, {'kind': 'error', 'error_kind': 'timeout', 't_send_ms': 0, 't_recv_ms': 0, 'path': path}
        with tempfile.TemporaryDirectory() as tmp:
            client = Dead()
            eng, jr, tw, t = make(tmp, client)
            ln, p = line(t)
            eng.handle_line(ln)
            self.assertEqual(jr.final_event(p['action_id'])['reason'], 'runtime_unreachable')
            self.assertEqual(client.calls, ['/status'])
            close_all(jr, tw)


class W11_Governance(unittest.TestCase):
    def project(self, tmp, text):
        os.makedirs(os.path.join(tmp, 'p'), exist_ok=True)
        write(os.path.join(tmp, 'p', 'project.toml'), text)
        return os.path.join(tmp, 'p')

    def targets(self, **over):
        t = dict(REG_DOC['targets'][0], key='flow')
        t.update(over)
        return [registry.parse(json.dumps({'version': 1, 'targets': [t]}), 'x').targets['flow']]

    def test_findings(self):
        gov = '[governance]\nwrite_mode = "allowlist"\n\n[[governance.rules]]\nvariable = "sp_flow"\nmin = 0.0\nmax = 40.0\n'
        with tempfile.TemporaryDirectory() as tmp:
            d = self.project(tmp, gov)
            self.assertEqual(governance_check.check(self.targets(), d), [])
            self.assertEqual([f['kind'] for f in governance_check.check(self.targets(max=50.0), d)], ['range_outside_governance'])
            self.assertEqual([f['kind'] for f in governance_check.check(self.targets(min=-1.0), d)], ['range_outside_governance'])
            self.assertEqual([f['kind'] for f in governance_check.check(self.targets(ia2_variable='other'), d)], ['not_in_governance'])
            self.assertEqual(governance_check.check(self.targets(enabled=False, max=999.0), d), [], 'disabled entries are not checked')
            d2 = self.project(tmp, '[governance]\nwrite_mode = "open"\n')
            self.assertEqual(governance_check.check(self.targets(ia2_variable='other'), d2), [])
            d3 = self.project(tmp, gov.replace('max = 40.0', 'max = nonsense'))
            self.assertEqual(len(governance_check.check(self.targets(), d3)), 1, 'an unreadable bound counts as a violation')
            with self.assertRaises(OSError):
                governance_check.check(self.targets(), os.path.join(tmp, 'nowhere'))

    def test_the_bridge_refuses_to_start_on_a_registry_outside_the_governance(self):
        with tempfile.TemporaryDirectory() as tmp:
            d = self.project(tmp, '[governance]\nwrite_mode = "allowlist"\n\n[[governance.rules]]\nvariable = "sp_flow"\nmin = 0.0\nmax = 40.0\n')
            reg = os.path.join(tmp, 'reg.json')
            doc = json.loads(json.dumps(REG_DOC))
            doc['targets'][0]['max'] = 50.0
            write(reg, json.dumps(doc))
            keys = os.path.join(tmp, 'keys.json')
            write(keys, json.dumps({'k1': {'secret_hex': '07' * 32, 'not_after_ms': 2 ** 50}}))
            os.chmod(keys, 0o600)
            import hashlib
            sha = hashlib.sha256(rb(reg)).hexdigest()
            base = [sys.executable, '-I', os.path.join(ROOT, 'bridge_run.py'), '--registry', reg, '--registry-sha256', sha, '--keys', keys, '--event-keys', event_keys_path(keys),
                    '--runtime', '127.0.0.1:1', '--journal', os.path.join(tmp, 'j'), '--requests', os.path.join(tmp, 'q'),
                    '--trace', os.path.join(tmp, 't'), '--project-dir', d, '--preflight', 'off']
            r = subprocess.run(base, capture_output=True, text=True, timeout=30)
            self.assertEqual(r.returncode, 5, r.stderr)
            self.assertIn('range_outside_governance', r.stderr)
            # a keys file others can read stops it before anything else
            os.chmod(keys, 0o644)
            r = subprocess.run(base, capture_output=True, text=True, timeout=30)
            self.assertEqual(r.returncode, 2)
            self.assertIn('readable by group or others', r.stderr)


class W12_EventIds(unittest.TestCase):
    def test_every_status_of_an_action_has_its_own_event_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            a = eng._event('a' * 32, {}, 'accepted', None)
            b = eng._event('a' * 32, {}, 'rejected', 'expired')
            c = eng._event('b' * 32, {}, 'accepted', None)
            self.assertEqual(len({a['event_id'], b['event_id'], c['event_id']}), 3)
            self.assertTrue(tw.emit(a) and tw.emit(b), 'a contradictory second event is written, not swallowed')
            close_all(jr, tw)


class W14_Durable(unittest.TestCase):
    def test_full_fsync_is_requested_where_the_platform_has_it(self):
        import fcntl
        calls = []
        real = fcntl.fcntl
        fcntl.fcntl = lambda fd, cmd, *a: calls.append(cmd) or 0
        try:
            with tempfile.TemporaryFile() as f:
                durable.sync(f.fileno())
        finally:
            fcntl.fcntl = real
        if hasattr(fcntl, 'F_FULLFSYNC'):
            self.assertEqual(calls, [fcntl.F_FULLFSYNC])
        with tempfile.TemporaryFile() as f:
            durable.sync(f.fileno())                                        # and it really works


class W15_Preflight(unittest.TestCase):
    class C:
        def __init__(self, status):
            self.status = status

        def get_json(self, path):
            return (self.status if path == '/status' else rc.golden('A0-health')), {}

    def reg(self):
        return registry.parse(json.dumps(REG_DOC), 'x')

    def status(self, **over):
        s = rc.golden('A0-status')
        s.update(runtime_id='R1', build_commit='abc')
        s['last_snapshot']['vars'] = [v for v in s['last_snapshot']['vars'] if v['name'] in ('sp_flow', 'sp_flow_fb')]
        s.update(over)
        return s

    def test_cases(self):
        reg = self.reg()
        self.assertEqual(preflight.check(self.C(self.status()), reg)['problems'], [])
        self.assertFalse(preflight.check(self.C(None), reg)['reachable'])
        p = preflight.check(self.C(self.status(runtime_id=None)), reg)['problems']
        self.assertTrue(p and 'runtime_id' in p[0] and 'legacy identity is NOT supported' in p[0])
        s = self.status()
        s['last_snapshot']['vars'][1].pop('input')
        self.assertIn('no input.stale', ' '.join(preflight.check(self.C(s), reg)['problems']))
        s = self.status()
        s['last_snapshot']['vars'].pop(1)
        self.assertIn('not in the runtime snapshot', ' '.join(preflight.check(self.C(s), reg)['problems']))

    def test_the_bridge_refuses_to_start_against_a_runtime_without_runtime_id(self):
        class H(BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps({'last_snapshot': {'vars': []}, 'build_commit': 'old'}).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *a):
                pass
        srv = HTTPServer(('127.0.0.1', 0), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                os.makedirs(os.path.join(tmp, 'p'))
                reg = os.path.join(tmp, 'reg.json')
                write(reg, json.dumps(REG_DOC))
                keys = os.path.join(tmp, 'keys.json')
                write(keys, json.dumps({'k1': {'secret_hex': '07' * 32, 'not_after_ms': 2 ** 50}}))
                os.chmod(keys, 0o600)
                import hashlib
                sha = hashlib.sha256(rb(reg)).hexdigest()
                r = subprocess.run([sys.executable, '-I', os.path.join(ROOT, 'bridge_run.py'), '--registry', reg, '--registry-sha256', sha, '--keys', keys, '--event-keys', event_keys_path(keys),
                                    '--runtime', '127.0.0.1:%d' % srv.server_address[1], '--journal', os.path.join(tmp, 'j'),
                                    '--requests', os.path.join(tmp, 'q'), '--trace', os.path.join(tmp, 't'), '--project-dir', os.path.join(tmp, 'p'),
                                    '--governance-check', 'off'], capture_output=True, text=True, timeout=30)
                self.assertEqual(r.returncode, 4, r.stderr)
                self.assertIn('runtime_id', r.stderr)
        finally:
            srv.shutdown()
            srv.server_close()


class W17_Visibility(unittest.TestCase):
    def test_counters_snapshot_and_a_rate_limited_notice(self):
        with tempfile.TemporaryDirectory() as tmp:
            eng, jr, tw, t = make(tmp)
            eng.handle_line('{"v":"ia2.control/v0","key_id":"k1","payload":"x","sig":"0"}')
            eng.handle_line('not json at all')
            nan = '{"target": NaN}'
            eng.handle_line(json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': nan, 'sig': auth.sign(KEYS['k1']['secret'], nan)}))
            snap = eng.status_snapshot()
            self.assertEqual((snap['counters']['unauthenticated'], snap['counters']['unattributed_rejects']), (2, 1))
            eng.maybe_notice()
            eng.maybe_notice()                                              # nothing changed, and not within a minute anyway
            notices = [decode_event(json.loads(l)) for l in lines(os.path.join(tmp, 't.jsonl')) if '"bridge_notice"' in l]
            self.assertEqual(len(notices), 1)
            self.assertEqual(notices[0]['counts']['unauthenticated'], 2)
            self.assertNotIn('payload', json.dumps(notices[0]), 'counts only, never content')
            eng.handle_line('still not json')
            eng.maybe_notice()
            self.assertEqual(len([l for l in lines(os.path.join(tmp, 't.jsonl')) if '"bridge_notice"' in l]), 1, 'at most one a minute')
            t[0] += 61000
            eng.maybe_notice()
            self.assertEqual(len([l for l in lines(os.path.join(tmp, 't.jsonl')) if '"bridge_notice"' in l]), 2)
            close_all(jr, tw)


class W18_SingleFlightWindow(unittest.TestCase):
    def test_an_accepted_action_frees_its_target_when_its_window_closes_not_at_a_far_deadline(self):
        jr_clock = [1000]
        with tempfile.TemporaryDirectory() as tmp:
            jr = Journal(os.path.join(tmp, 'j'), lambda: jr_clock[0])
            jr.open()
            jr.append('intent', action_id='a1', req_hash='h', target_key='k', deadline_ms=1_000_000, label='tier0.x.a1', info={})
            jr.append('terminal', action_id='a1', event={'status': 'accepted'})
            self.assertEqual(jr.open_for_target('k', 1500, 5000), ['a1'], 'inside the window')
            self.assertEqual(jr.open_for_target('k', 6500, 5000), [], 'the window (5 s) has closed although the deadline is far away')
            self.assertEqual(jr.open_for_target('k', 6500), ['a1'], 'without a window the old behaviour: until the deadline')
            jr.close()

    def test_registry_default_window_follows_the_verification_window(self):
        reg = registry.parse(json.dumps(REG_DOC), 'x')
        self.assertEqual(reg.targets['flow']['single_flight_ms'], 5000)
        doc = json.loads(json.dumps(REG_DOC))
        del doc['targets'][0]['verify']
        self.assertEqual(registry.parse(json.dumps(doc), 'x').targets['flow']['single_flight_ms'], 2000)
        doc['targets'][0]['single_flight_ms'] = 750
        self.assertEqual(registry.parse(json.dumps(doc), 'x').targets['flow']['single_flight_ms'], 750)


class W19_Horizon(unittest.TestCase):
    def test_old_action_details_expire_but_occupancy_chain_and_sets_still_load(self):
        with tempfile.TemporaryDirectory() as tmp:
            now = [10_000_000_000]
            jr = Journal(os.path.join(tmp, 'j'), lambda: now[0], horizon_ms=1000)
            jr.open()
            jr.append('intent', action_id='old', req_hash='h', target_key='k', deadline_ms=1, label='tier0.x.old', info={})
            jr.append('terminal', action_id='old', event={'status': 'accepted'})
            jr.append('disable', target_key='k', reason='r', action_id='old')
            jr.append('internal_error', line_sha256='q' * 64, error='E', detail='d', where='w')
            now[0] += 5000
            jr.append('intent', action_id='new', req_hash='h2', target_key='k2', deadline_ms=now[0] + 9000, label='tier0.x.new', info={})
            jr.close()
            jr2 = Journal(os.path.join(tmp, 'j'), lambda: now[0], horizon_ms=1000)
            jr2.open()
            self.assertEqual(sorted(jr2.actions), ['new'])
            self.assertTrue(jr2.has_action('old'))
            self.assertEqual(jr2.request_hash('old'), 'h')
            self.assertEqual(jr2.final_event('old'), {'status': 'accepted'})
            self.assertTrue(jr2.label_in_use('tier0.x.old', 'different'))
            self.assertIn('k', jr2.disabled)
            self.assertIn('q' * 64, jr2.quarantined)
            self.assertEqual(jr2.seq, 5)
            jr2.close()


if __name__ == '__main__':
    unittest.main()
