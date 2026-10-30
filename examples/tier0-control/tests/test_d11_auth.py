"""D11 real authentication boundary: attack inputs never pass fixture sign adapters."""
import copy
import hashlib
import hmac
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path[:0] = [str(Path(__file__).resolve().parent), str(Path(__file__).resolve().parents[1])]
from signing_fixtures import EVENT_KEYS, SAMPLE_KEYS, event_record, sample_record
import test_verifier as tv
from bridge import eventauth
from bridge.trace import TraceWriter
from readside import auth, verifier


class Authentication(unittest.TestCase):
    def setUp(self):
        self.event = event_record(tv.ACC)
        self.sample = sample_record(tv.sample())
        self.events = [eventauth.sign(self.event, EVENT_KEYS)]
        self.samples = [auth.sign(self.sample, SAMPLE_KEYS, 'sample')]

    def derive(self, events=None, samples=None, **kw):
        return verifier.derive(tv.REQ, tv.ENTRY, self.events if events is None else events,
                               self.samples if samples is None else samples, tv.AS_OF,
                               registry_sha256=self.event['registry_sha256'],
                               event_keys=EVENT_KEYS, sample_keys=SAMPLE_KEYS, **kw)

    def test_independent_bridge_signature_and_readside_agree(self):
        self.assertEqual(self.derive()['status'], 'verified')
        env = self.events[0]
        expected = hmac.new(b'E' * 32, b'ia2.event/v0\n' + env['payload'].encode(), hashlib.sha256).hexdigest()
        self.assertEqual(env['sig'], expected)
        self.assertEqual(auth.verify(env, EVENT_KEYS, 'event'), self.event)
        self.assertEqual(eventauth.verify(json.dumps(env), EVENT_KEYS), self.event)

    def test_F6_unsigned_accepted_is_not_a_bridge_event(self):
        r = self.derive(events=[self.event])
        self.assertEqual((r['status'], r['rejected_envelopes']), ('requested', 1))

    def test_F7_unsigned_matching_sample_cannot_verify(self):
        r = self.derive(samples=[self.sample])
        self.assertNotEqual(r['status'], 'verified')
        self.assertEqual(r['rejected_by_role']['sample'], 1)

    def test_F8_both_unsigned_cannot_verify(self):
        r = self.derive(events=[self.event], samples=[self.sample])
        self.assertEqual((r['status'], r['rejected_envelopes']), ('requested', 2))

    def test_each_role_rejects_forgery_variants(self):
        for role, good in (('event', self.events[0]), ('sample', self.samples[0])):
            variants = [None, [], 7, 'not json', dict(good, sig='0' * 64), dict(good, key_id='unknown'),
                        dict(good, payload=good['payload'] + ' '), dict(good, extra=True),
                        dict(good, v='ia2.control/v0'), dict(good, hint={'event_id': 'forged'}),
                        dict(good, payload=[]), dict(good, sig='a' * 63 + '\n'), dict(good, key_id=[])]
            for bad in variants:
                with self.subTest(role=role, bad=str(bad)[:70]):
                    r = self.derive(**({'events': [bad]} if role == 'event' else {'samples': [bad]}))
                    self.assertNotEqual(r['status'], 'verified')
                    self.assertEqual(r['rejected_by_role'][role], 1)

    def test_wrong_role_secret_and_domain_never_substitute(self):
        wrong = copy.deepcopy(self.events[0])
        for secret, prefix in ((b'S' * 32, b'ia2.event/v0\n'), (b'E' * 32, b'ia2.control/v0\n')):
            wrong['sig'] = hmac.new(secret, prefix + wrong['payload'].encode(), hashlib.sha256).hexdigest()
            self.assertEqual(self.derive(events=[wrong])['status'], 'requested')
        sample = auth.sign(self.sample, EVENT_KEYS, 'sample')
        self.assertNotEqual(self.derive(samples=[sample])['status'], 'verified')

    def test_rejected_rows_do_not_mask_valid_evidence(self):
        r = self.derive(events=[dict(self.events[0], sig='0' * 64)] + self.events,
                        samples=[self.sample] + self.samples)
        self.assertEqual((r['status'], r['rejected_envelopes']), ('verified', 2))

    def test_revocations_apply_to_both_roles(self):
        for kid in ('event-test', 'sample-test'):
            r = self.derive(revoked={kid})
            self.assertNotEqual(r['status'], 'verified')
            self.assertEqual(r['rejected_envelopes'], 1)

    def test_historical_keys_and_window_boundaries(self):
        for role, rec, original in (('event', self.event, EVENT_KEYS), ('sample', self.sample, SAMPLE_KEYS)):
            stamp = rec['observed_at_ms' if role == 'event' else 'collected_at_ms']
            keys = copy.deepcopy(original)
            kid = next(iter(keys))
            keys[kid].update(not_before_ms=stamp, not_after_ms=stamp)
            signed = auth.sign(rec, keys, role)
            try:
                verified = auth.verify(signed, keys, role)
            except ValueError:
                self.fail('valid historical evidence must survive key expiry; do not use the current time')
            self.assertEqual(verified, rec)
            for lo, hi in ((stamp + 1, stamp + 100), (0, stamp - 1)):
                keys[kid].update(not_before_ms=lo, not_after_ms=hi)
                with self.assertRaises(ValueError):
                    auth.verify(signed, keys, role)

    def test_rotation_uses_latest_valid_nonrevoked_key(self):
        keys = dict(EVENT_KEYS, newer=dict(secret=b'N' * 32, not_before_ms=1000, not_after_ms=2000))
        self.assertEqual(eventauth.sign(self.event, keys)['key_id'], 'newer')
        self.assertEqual(auth.sign(self.event, keys, 'event')['key_id'], 'newer')
        self.assertEqual(auth.sign(self.event, keys, 'event', {'newer'})['key_id'], 'event-test')

    def test_no_unsigned_mode_or_shared_role_keys(self):
        with self.assertRaises(TypeError):
            verifier.derive(tv.REQ, tv.ENTRY, [], [], tv.AS_OF)
        for events, samples in (({}, SAMPLE_KEYS), (EVENT_KEYS, {}), (EVENT_KEYS, EVENT_KEYS)):
            with self.assertRaises(ValueError):
                verifier.derive(tv.REQ, tv.ENTRY, [], [], tv.AS_OF, event_keys=events, sample_keys=samples)

    def test_signed_malformed_json_and_schema_are_rejected(self):
        good = self.events[0]
        for text in ('{"x":1,"x":2}', '[1]', '{"x":NaN}', '{"x":1e999}', '[' * 2000,
                     json.dumps(dict(self.event, schema='foreign')), json.dumps(dict(self.event, kind='unknown')),
                     json.dumps(dict(self.event, observed_at_ms=True))):
            raw = dict(good, payload=text)
            raw['sig'] = hmac.new(b'E' * 32, b'ia2.event/v0\n' + text.encode(), hashlib.sha256).hexdigest()
            self.assertEqual(self.derive(events=[raw])['rejected_envelopes'], 1)
        duplicate = json.dumps(good)[:-1] + ', "v":"ia2.event/v0"}'
        self.assertEqual(self.derive(events=[duplicate])['rejected_envelopes'], 1)

    def test_unsigned_anchor_cannot_suppress_recovery(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp, 'trace.jsonl')
            fake = dict(self.event, journal_seq=999, journal_hash='f' * 64)
            path.write_text(json.dumps(fake) + '\n')
            writer = TraceWriter(str(path), EVENT_KEYS)
            try:
                self.assertIsNone(writer.last_anchor)
                self.assertNotIn(fake['event_id'], writer.seen)
                self.assertTrue(writer.emit(self.event, (1, 'a' * 64)))
            finally:
                writer.close()
            writer = TraceWriter(str(path), EVENT_KEYS)
            try:
                self.assertEqual(writer.last_anchor, (1, 'a' * 64))
                self.assertFalse(writer.emit(self.event))
            finally:
                writer.close()

    def test_signed_invalid_json_values_cannot_supply_recovery_identity_or_anchor(self):
        event = dict(self.event, journal_seq=999, journal_hash='f' * 64)
        base = json.dumps(event)[:-1]
        for suffix in (',"extra":1e999}', ',"extra":{"value":-1e999}}'):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as tmp:
                payload = base + suffix
                env = dict(self.events[0], payload=payload)
                env['sig'] = hmac.new(b'E' * 32, b'ia2.event/v0\n' + payload.encode(), hashlib.sha256).hexdigest()
                with self.assertRaises(ValueError):
                    auth.verify(env, EVENT_KEYS, 'event')
                with self.assertRaises(ValueError):
                    eventauth.verify(json.dumps(env), EVENT_KEYS)
                path = Path(tmp, 'trace.jsonl')
                path.write_text(json.dumps(env) + '\n')
                writer = TraceWriter(str(path), EVENT_KEYS)
                try:
                    self.assertIsNone(writer.last_anchor)
                    self.assertNotIn(event['event_id'], writer.seen)
                    self.assertTrue(writer.emit(self.event))
                finally:
                    writer.close()

    def test_json_escaped_diagnostic_strings_survive_signing_and_recovery(self):
        event = dict(self.event, detail='\udfff 温度 \x00 \U0001f600')
        signed = eventauth.sign(event, EVENT_KEYS)
        wire = json.dumps(signed).encode('utf-8')
        self.assertEqual(auth.verify(wire, EVENT_KEYS, 'event'), event)
        self.assertEqual(eventauth.verify(wire.decode('utf-8'), EVENT_KEYS), event)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp, 'trace.jsonl')
            writer = TraceWriter(str(path), EVENT_KEYS)
            try:
                self.assertTrue(writer.emit(event))
            finally:
                writer.close()
            writer = TraceWriter(str(path), EVENT_KEYS)
            try:
                self.assertEqual(writer.skipped, [])
                self.assertFalse(writer.emit(event))
            finally:
                writer.close()

    def test_revoked_anchor_is_not_trusted_at_restart(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = str(Path(tmp, 'trace.jsonl'))
            writer = TraceWriter(path, EVENT_KEYS)
            writer.emit(self.event, (7, 'b' * 64))
            writer.close()
            writer = TraceWriter(path, EVENT_KEYS, {'event-test'})
            try:
                self.assertEqual(writer.skipped, [1])
                self.assertIsNone(writer.last_anchor)
            finally:
                writer.close()

    def test_key_files_require_private_mode_and_valid_material(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp, 'keys.json')
            spec = {'k': dict(secret_hex='ee' * 32, not_before_ms=0, not_after_ms=100)}
            path.write_text(json.dumps(spec))
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                auth.load_keys(path)
            path.chmod(0o600)
            self.assertEqual(auth.load_keys(path)['k']['secret'], b'\xee' * 32)
            spec['k']['not_before_ms'] = True
            path.write_text(json.dumps(spec))
            with self.assertRaises(ValueError):
                auth.load_keys(path)

    def test_overflowing_request_has_a_signed_rejection_not_pending_publication(self):
        from p0_support import engine, payload, close, KEYS
        from bridge.auth import sign
        for number in ('1e999', '-1e999'):
            with self.subTest(number=number), tempfile.TemporaryDirectory() as tmp:
                clock = [1800000000000]
                eng = engine(tmp, clock)
                try:
                    p = payload(clock[0], target=1)
                    text = json.dumps(p).replace('"target": 1,', '"target": ' + number + ',')
                    line = json.dumps(dict(v='ia2.control/v0', key_id='k', payload=text, sig=sign(KEYS['k']['secret'], text)))
                    eng.safe_handle_line(line)
                    rec = auth.verify(Path(tmp, 'trace.jsonl').read_text(), EVENT_KEYS, 'event')
                    self.assertEqual((rec['status'], rec['reason'], rec['requested_value']), ('rejected', 'bad_value', None))
                    self.assertEqual(eng.client.posts, [])
                    self.assertEqual(eng.status_snapshot()['pending_trace_results'], 0)
                finally:
                    close(eng)



if __name__ == '__main__':
    unittest.main(verbosity=2)
