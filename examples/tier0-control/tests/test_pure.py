"""Unit tests for the bridge's pure layer.  Run: python3 -I -m unittest discover -s tests -t .  (from the 1a directory)"""
import json
import os
import struct
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)

from bridge import auth, classify, gates, northbound_guard, registry, strictjson, values  # noqa: E402
from bridge.errors import Reject  # noqa: E402
from bridge.journal import Journal, JournalError  # noqa: E402

GOLDEN = os.path.join(ROOT, 'tests', 'fixtures', 'golden')


def golden(name):
    with open(os.path.join(GOLDEN, name + '.json'), encoding='utf-8') as f:
        rec = json.load(f)
    rec['kind'] = 'response'
    rec['text'] = json.dumps(rec['body']) if rec.get('body') is not None else rec.get('raw')
    rec['json'] = rec.get('body')
    return rec


def write(path, data, mode='w'):
    with open(path, mode) as f:
        f.write(data)


def read_bytes(path):
    with open(path, 'rb') as f:
        return f.read()


def entry(**kw):
    e = {'key': 'k', 'device_id': 'd', 'ia2_variable': 'v', 'type': 'REAL', 'min': 0.0, 'max': 40.0, 'simulation': True,
         'enabled': True, 'requires_approval': False, 'require_devices': ['coupler'], 'require_fieldbus_healthy': True,
         'max_rate_per_min': 6, 'min_interval_ms': 2000, 'max_deadline_ms': 60000, 'verify': None, 'note': ''}
    e.update(kw)
    return e


class StrictJson(unittest.TestCase):
    def test_duplicates_and_nonfinite_refused(self):
        for text in ('{"a":1,"a":2}', '{"a":NaN}', '{"a":Infinity}', '[1,', ''):
            with self.assertRaises(strictjson.StrictJSONError):
                strictjson.loads(text)
        self.assertEqual(strictjson.loads('{"a":[1,2.5,true,null]}'), {'a': [1, 2.5, True, None]})


class Auth(unittest.TestCase):
    keys = {'k1': {'secret': b'\x01' * 32, 'not_before_ms': 0, 'not_after_ms': 2 ** 50}}

    def env(self, payload='{"x":1}', key_id='k1', secret=b'\x01' * 32, **over):
        e = {'v': 'ia2.control/v0', 'key_id': key_id, 'payload': payload,
             'sig': auth.sign(secret, payload) if isinstance(payload, str) else ''}
        e.update(over)
        return json.dumps(e)

    def reason(self, line, now=1000):
        with self.assertRaises(Reject) as cm:
            auth.verify_envelope(line, self.keys, now)
        return cm.exception.reason

    def test_good(self):
        self.assertEqual(auth.verify_envelope(self.env(), self.keys, 1000), ('{"x":1}', 'k1'))

    def test_bad_signature_variants(self):
        self.assertEqual(self.reason(self.env(secret=b'\x02' * 32)), 'unauthenticated')
        self.assertEqual(self.reason(self.env(sig='0' * 64)), 'unauthenticated')
        self.assertEqual(self.reason(self.env(sig='')), 'unauthenticated')
        self.assertEqual(self.reason(self.env(key_id='nope')), 'unauthenticated')
        tampered = json.loads(self.env())
        tampered['payload'] = '{"x":2}'                     # one byte changed, original signature kept
        self.assertEqual(self.reason(json.dumps(tampered)), 'unauthenticated')

    def test_whitespace_in_payload_changes_the_signature(self):
        # D10: the signature covers the exact bytes, so a re-serialisation is a different message.
        e = json.loads(self.env('{"x":1}'))
        e['payload'] = '{"x": 1}'
        self.assertEqual(self.reason(json.dumps(e)), 'unauthenticated')

    def test_malformed_envelopes(self):
        for line in ('not json', '[]', json.dumps({'v': 'ia2.control/v0'}), self.env(extra=1),
                     self.env(v='other'), self.env(payload=5)):
            self.assertEqual(self.reason(line), 'bad_request', line[:50])

    def test_key_window(self):
        keys = {'k1': {'secret': b'\x01' * 32, 'not_before_ms': 500, 'not_after_ms': 900}}
        for now in (499, 901):
            with self.assertRaises(Reject):
                auth.verify_envelope(self.env(), keys, now)
        auth.verify_envelope(self.env(), keys, 700)

    def test_error_text_never_contains_secret_or_sig(self):
        line = self.env(sig='a' * 64)
        with self.assertRaises(Reject) as cm:
            auth.verify_envelope(line, self.keys, 1000)
        self.assertNotIn('a' * 64, str(cm.exception))
        self.assertNotIn(('01' * 32), str(cm.exception))


class Registry(unittest.TestCase):
    def doc(self, **over):
        t = {'key': 'flow', 'device_id': 'd', 'ia2_variable': 'sp_flow', 'type': 'REAL', 'min': 0, 'max': 40,
             'simulation': True, 'enabled': True}
        t.update(over)
        return json.dumps({'version': 1, 'targets': [t]})

    def test_good_and_defaults(self):
        r = registry.parse(self.doc(), 'x')
        self.assertEqual(r.targets['flow']['max_rate_per_min'], 6)

    def test_refusals(self):
        for over in ({'bogus': 1}, {'type': 'LREAL'}, {'min': 5, 'max': 1}, {'key': 'bad key'}, {'enabled': 'yes'},
                     {'verify': {'feedback': 'x'}}, {'requires_approval': True}):
            with self.assertRaises(registry.RegistryError, msg=str(over)):
                registry.parse(self.doc(**over), 'x')

    def test_disabled_entry_may_keep_the_reserved_field(self):
        registry.parse(self.doc(requires_approval=True, enabled=False), 'x')

    def test_duplicate_key_and_json_duplicates(self):
        t = json.loads(self.doc())['targets'][0]
        with self.assertRaises(registry.RegistryError):
            registry.parse(json.dumps({'version': 1, 'targets': [t, t]}), 'x')
        with self.assertRaises(registry.RegistryError):
            registry.parse('{"version":1,"version":1,"targets":[]}', 'x')

    def test_pin(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, 'r.json')
            write(p, self.doc())
            import hashlib
            sha = hashlib.sha256(read_bytes(p)).hexdigest()
            registry.load_pinned(p, sha)
            write(p, ' ', 'a')
            with self.assertRaises(registry.RegistryError):
                registry.load_pinned(p, sha)


class Values(unittest.TestCase):
    def test_real_bits_match_wp0(self):
        self.assertEqual(values.validate_and_encode(entry(), 12.5)[0], 1095237632)            # WP0 B01
        self.assertEqual(values.validate_and_encode(entry(), 12.3456789)[0], 1095075815)      # WP0 B03, 0x414587e7
        self.assertEqual(values.validate_and_encode(entry(), 40)[0], 1109393408)

    def test_range_checks_before_and_after_rounding(self):
        for bad in (-0.0001, 40.000001, 45):
            with self.assertRaises(Reject) as cm:
                values.validate_and_encode(entry(), bad)
            self.assertEqual(cm.exception.reason, 'out_of_range')
        with self.assertRaises(Reject) as cm:               # 0.1 in double is below max, but its f32 is above it
            values.validate_and_encode(entry(min=0, max=0.1), 0.1)
        self.assertEqual(cm.exception.reason, 'out_of_range')

    def test_negative_zero_is_refused(self):
        with self.assertRaises(Reject) as cm:
            values.validate_and_encode(entry(), -0.0)
        self.assertEqual(cm.exception.reason, 'bad_value')
        values.validate_and_encode(entry(), 0.0)
        values.validate_and_encode(entry(), 0)

    def test_non_numbers_and_non_finite(self):
        for bad in ('12', None, [1], {'a': 1}, True, float('inf'), float('-inf'), float('nan')):
            with self.assertRaises(Reject) as cm:
                values.validate_and_encode(entry(), bad)
            self.assertEqual(cm.exception.reason, 'bad_value', repr(bad))

    def test_bool_is_stricter_than_ia2(self):
        e = entry(type='BOOL', min=0, max=1)
        for ok in (True, False, 0, 1, 0.0, 1.0):
            values.validate_and_encode(e, ok)
        for bad in (2, -1, 0.5, 'true', None):
            with self.assertRaises(Reject):
                values.validate_and_encode(e, bad)

    def test_integers(self):
        e = entry(type='INT', min=0, max=100)
        self.assertEqual(values.validate_and_encode(e, 42), (42, 42))
        self.assertEqual(values.validate_and_encode(e, 42.0), (42, 42))
        with self.assertRaises(Reject):
            values.validate_and_encode(e, 42.5)
        with self.assertRaises(Reject):
            values.validate_and_encode(entry(type='INT', min=0, max=100000), 40000)    # outside INT
        self.assertEqual(values.validate_and_encode(entry(type='DINT', min=-1000, max=1000), -5), (-5, -5))


class Payload(unittest.TestCase):
    def good(self, **over):
        p = {'event_id': 'e', 'action_id': 'a' * 32, 'session_id': 's', 'device_id': 'd', 'command': 'ia2_set',
             'target_key': 'k', 'target': 1.5, 'simulation': True, 'requested_at_ms': 1000, 'deadline_ms': 5000,
             'requested_by': 'agent:x'}
        p.update(over)
        return p

    def test_good(self):
        gates.validate_payload(self.good())

    def test_unknown_missing_wrongtype_and_other_commands(self):
        for command in ('force', 'unforce', 'stop', 'pause', 'resume', 'inject', 'alarm_ack', 'pulse', ''):
            with self.assertRaises(Reject) as cm:
                gates.validate_payload(self.good(command=command))
            self.assertEqual((cm.exception.reason, cm.exception.detail), ('bad_request', 'unsupported_command'))
        bad = [dict(self.good(), pulse_ms=100), {k: v for k, v in self.good().items() if k != 'target'},
               self.good(simulation=1), self.good(requested_at_ms=1000.0), self.good(requested_at_ms=True),
               self.good(action_id='A' * 32), self.good(action_id='a' * 31), self.good(target='x'), self.good(target=None)]
        for obj in bad:
            with self.assertRaises(Reject) as cm:
                gates.validate_payload(obj)
            self.assertEqual(cm.exception.reason, 'bad_request')

    def test_ids_are_extracted_even_from_bad_payloads(self):
        ids = gates.extract_ids(self.good(command='force'))
        self.assertEqual(ids['action_id'], 'a' * 32)
        self.assertIsNone(gates.extract_ids({'action_id': 'nope'})['action_id'])
        self.assertIsNone(gates.extract_ids([1])['action_id'])


class Gates(unittest.TestCase):
    reg = registry.parse(json.dumps({'version': 1, 'targets': [
        {'key': 'k', 'device_id': 'd', 'ia2_variable': 'v', 'type': 'REAL', 'min': 0, 'max': 40, 'simulation': True,
         'enabled': True, 'require_devices': ['coupler']}]}), 'x')

    def p(self, **over):
        return Payload().good(**over)

    def test_registry_gate(self):
        self.assertEqual(gates.check_registry(self.p(), self.reg, set())['key'], 'k')
        for payload, reason in ((self.p(target_key='sp_flow'), 'not_in_registry'),
                                (self.p(device_id='other'), 'not_in_registry'),
                                (self.p(simulation=False), 'simulation_mismatch')):
            with self.assertRaises(Reject) as cm:
                gates.check_registry(payload, self.reg, set())
            self.assertEqual(cm.exception.reason, reason)
        with self.assertRaises(Reject):
            gates.check_registry(self.p(), self.reg, {'k'})

    def test_time_gate(self):
        e = self.reg.targets['k']
        gates.check_time(self.p(requested_at_ms=10000, deadline_ms=20000), e, 10500)
        for kw in ({'deadline_ms': 10000}, {'deadline_ms': 10500 + 60001}, {'requested_at_ms': 13000},
                   {'requested_at_ms': 7000}):
            with self.assertRaises(Reject) as cm:
                gates.check_time(self.p(**dict({'requested_at_ms': 10000, 'deadline_ms': 20000}, **kw)), e, 10500)
            self.assertEqual(cm.exception.reason, 'expired', kw)

    def test_hold(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, 'hold')
            gates.check_hold(path)
            write(path, '')
            with self.assertRaises(Reject):
                gates.check_hold(path)

    # ---- G8 / G9 on shapes copied from the WP0 golden samples ----------------------------------------------------------
    @staticmethod
    def health(**over):
        h = golden('A0-health')['body']
        h.update(over)
        return h

    @staticmethod
    def status(**over):
        s = golden('A0-status')['body']
        s.update(over)
        return s

    def test_g8_clean(self):
        a, b = self.status(scan_count=10), self.status(scan_count=20)
        self.assertEqual(gates.check_runtime(self.health(), a, b), b['runtime_id'])

    def test_g8_refusals(self):
        a, b = self.status(scan_count=10), self.status(scan_count=20)
        cases = [
            (self.health(watchdog_tripped=True), a, b, 'latched'),
            (self.health(latch_reason='x'), a, b, 'latched'),
            (self.health(failsafe_status={'pending': ['coupler'], 'acknowledged': [], 'unconfirmed': []}), a, b, 'latched'),
            (self.health(fault='boom'), a, b, 'vm_error'),
            (self.health(status='degraded'), a, b, 'runtime_unreachable'),
            (self.health(snapshot_tap={'running': False}), a, b, 'runtime_unreachable'),
            (self.health(), a, self.status(scan_count=10), 'runtime_unreachable'),                      # not advancing
            (self.health(), a, self.status(scan_count=20, runtime_id='other'), 'runtime_unreachable'),   # restarted
            (self.health(), a, self.status(scan_count=20, runtime_id=None), 'runtime_unreachable'),
            (self.health(), a, self.status(scan_count=20, watchdog_tripped=True), 'latched'),
            (None, a, b, 'runtime_unreachable'),
        ]
        for h, sa, sb, reason in cases:
            with self.assertRaises(Reject) as cm:
                gates.check_runtime(h, sa, sb)
            self.assertEqual(cm.exception.reason, reason, (h, reason))

    def test_g8_can_be_skipped_for_diagnosis_but_identity_is_still_required(self):
        a, b = self.status(scan_count=10), self.status(scan_count=20)
        gates.check_runtime(self.health(watchdog_tripped=True), a, b, skip_g8=True)
        with self.assertRaises(Reject):
            gates.check_runtime(self.health(), a, self.status(scan_count=20, runtime_id='other'), skip_g8=True)

    def test_g9(self):
        e = self.reg.targets['k']
        gates.check_target(self.health(), self.status(), e)
        cases = [(self.status(mode={'kind': 'paused'}), 'paused'),
                 (self.status(forces=[{'name': 'v', 'value': 1}]), 'forced'),
                 (self.status(forces=[{'weird': 1}]), 'runtime_unreachable'),
                 (self.status(device_health=[{'name': 'coupler', 'healthy': False}]), 'device_unhealthy'),
                 (self.status(device_health=[]), 'device_unhealthy')]
        for st, reason in cases:
            with self.assertRaises(Reject) as cm:
                gates.check_target(self.health(), st, e)
            self.assertEqual(cm.exception.reason, reason)
        with self.assertRaises(Reject) as cm:
            gates.check_target(self.health(fieldbus_healthy=False), self.status(), e)
        self.assertEqual(cm.exception.reason, 'fieldbus_unhealthy')


class Classify(unittest.TestCase):
    """The rows below are the REAL responses recorded in WP0 (tests/fixtures/golden)."""

    def c(self, name, bits, var):
        return classify.classify_write(golden(name), bits, var)

    def test_wp0_responses(self):
        r = self.c('B01-real-12.5', 1095237632, 'sp_flow')
        self.assertEqual((r['state'], r['reason']), ('accepted', None))
        r = self.c('B02-real-45-clamp', 1110704128, 'sp_flow')                    # asked 45.0, IA2 applied 40.0
        self.assertEqual((r['state'], r['reason'], r['applied_bits']), ('modified', 'clamped', 1109393408))
        r = self.c('B04-real-nan', 2143289344, 'sp_flow')
        self.assertEqual((r['state'], r['reason']), ('rejected', 'governance_denied'))
        r = self.c('B15-unknown-var', 1, 'no_such_variable')
        self.assertEqual((r['state'], r['reason'], r.get('disable_target')), ('rejected', 'unknown_variable', True))
        r = self.c('C02-write-while-down', 1084227584, 'd2_sp')
        self.assertEqual((r['state'], r['reason'], r['undelivered_device']), ('undelivered', 'link_down', 'second'))
        r = self.c('D02-write-after-stop', 1077936128, 'sp_flow')
        self.assertEqual((r['state'], r['reason']), ('rejected', 'scan_stopped'))

    def test_missing_key_is_not_null(self):
        rec = golden('B01-real-12.5')
        del rec['json']['undelivered_device']
        r = classify.classify_write(rec, 1095237632, 'sp_flow')
        self.assertEqual((r['state'], r['reason']), ('unknown', 'vm_error'))

    def test_hostile_or_odd_responses_are_unknown(self):
        def resp(status, json_body=None, text=''):
            return {'kind': 'response', 'status': status, 'json': json_body, 'text': text}
        ok = {'ok': True, 'name': 'v', 'value': 5, 'undelivered_device': None}
        cases = [resp(500), resp(302), resp(200), resp(200, [1], '[1]'), resp(200, dict(ok, ok=False)),
                 resp(200, dict(ok, name='other')), resp(200, dict(ok, value='5')), resp(200, dict(ok, value=True)),
                 resp(200, dict(ok, undelivered_device=3)), resp(403, text='forbidden by the origin guard'),
                 resp(404, text='no route'), resp(409, text='something else')]
        for res in cases:
            self.assertEqual(classify.classify_write(res, 5, 'v')['state'], 'unknown', res)

    def test_transport_errors_are_unknown(self):
        for kind in ('timeout', 'connect', 'reset', 'truncated', 'protocol', 'other'):
            r = classify.classify_write({'kind': 'error', 'error_kind': kind}, 5, 'v')
            self.assertEqual((r['state'], r['reason']), ('unknown', 'runtime_unreachable'))

    def test_clamped_and_undelivered_keeps_both_facts(self):
        res = {'kind': 'response', 'status': 200, 'text': '', 'json': {'ok': True, 'name': 'v', 'value': 4, 'undelivered_device': 'x'}}
        r = classify.classify_write(res, 5, 'v')
        self.assertEqual((r['state'], r['also_modified']), ('undelivered', True))


class NorthboundGuard(unittest.TestCase):
    def check(self, text):
        with tempfile.TemporaryDirectory() as d:
            if text is not None:
                write(os.path.join(d, 'northbound.toml'), text)
            return northbound_guard.check(d)

    def test_absent_and_false_pass(self):
        self.assertEqual(self.check(None), 'absent')
        self.check('[mqtt]\nbroker_host = "x"\nallow_write = false\n')
        self.check('[mqtt]\nbroker_host = "x"\n# allow_write = true\n')
        self.check('mqtt = { broker_host = "x", allow_write = false }\n')

    def test_everything_else_refuses(self):
        for text in ('[mqtt]\nallow_write = true\n', '[mqtt]\nallow_write=true\n', '[mqtt]\nallow_write = 1\n',
                     'mqtt = { allow_write = true }\n', '[mqtt]\n"allow_write" = true\n', '[mqtt]\nallow_write = false_ish\n'):
            with self.assertRaises(Reject, msg=text) as cm:
                self.check(text)
            self.assertEqual(cm.exception.reason, 'northbound_write_enabled')

    def test_unreadable_refuses(self):
        with tempfile.TemporaryDirectory() as d:
            write(os.path.join(d, 'northbound.toml'), b'\xff\xfe\x00', 'wb')
            with self.assertRaises(Reject):
                northbound_guard.check(d)


class JournalTests(unittest.TestCase):
    def new(self, d):
        t = [1000]

        def clock():
            t[0] += 1
            return t[0]
        j = Journal(os.path.join(d, 'j.jsonl'), clock)
        j.open()
        return j

    def test_replay_rebuilds_the_index(self):
        with tempfile.TemporaryDirectory() as d:
            j = self.new(d)
            j.append('intent', action_id='a1', req_hash='h', target_key='k', deadline_ms=9999, label='tier0.x.a1', info={})
            j.append('terminal', action_id='a1', event={'status': 'accepted'})
            j.append('disable', target_key='bad', action_id='a1')
            j.close()
            j2 = self.new(d)
            self.assertEqual(j2.actions['a1']['req_hash'], 'h')
            self.assertEqual(j2.final_event('a1'), {'status': 'accepted'})
            self.assertIn('bad', j2.disabled)
            self.assertEqual(j2.open_for_target('k', 5000), ['a1'])      # accepted and before its deadline
            self.assertEqual(j2.open_for_target('k', 10000), [])         # past the deadline
            self.assertEqual(len(j2.intent_times(0, 'k')), 1)
            j2.close()

    def test_torn_tail_is_copied_aside_and_cut(self):
        with tempfile.TemporaryDirectory() as d:
            j = self.new(d)
            j.append('start')
            j.close()
            path = os.path.join(d, 'j.jsonl')
            with open(path, 'ab') as f:
                f.write(b'{"seq":2,"t_ms":1,"prev":"abc","kind":"inte')       # killed mid-append
            j2 = self.new(d)
            self.assertIsNotNone(j2.torn_copy)
            self.assertTrue(os.path.exists(j2.torn_copy))
            self.assertEqual(j2.seq, 1)
            j2.append('start')
            j2.close()
            self.new(d).close()                                                  # chain still verifies

    def test_broken_chain_refuses(self):
        with tempfile.TemporaryDirectory() as d:
            j = self.new(d)
            j.append('start')
            j.append('start')
            j.close()
            path = os.path.join(d, 'j.jsonl')
            lines = read_bytes(path).split(b'\n')
            lines[0] = lines[0].replace(b'"kind":"start"', b'"kind":"stArt"')
            write(path, b'\n'.join(lines), 'wb')
            with self.assertRaises(JournalError):
                self.new(d)


if __name__ == '__main__':
    unittest.main()
