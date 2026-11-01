"""P1 F05 / F06: the read-side verifier compares EXACT numbers according to the registry's type, and a value that is not a finite
number of the right kind never proves anything.  (F05: a feedback of +-infinity was `verified`; F06: the tolerance grew with the
magnitude of the numbers, so a DINT feedback one count off passed with tolerance 0.)

The expected results are written out as tables and re-checked against a second arithmetic engine (decimal, with far more digits than
any number here needs).  Nothing in this file calls the verifier's own comparison helpers.  These vectors isolate arithmetic AFTER authentication; test_d11_auth.py checks the mandatory authentication boundary. No POST is involved."""
import decimal
import math
import os
import sys
import unittest

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path[:0] = [os.path.dirname(HERE), HERE, os.path.join(os.path.dirname(HERE), 'lab')]
import sampler  # noqa: E402
import test_verifier as tv  # noqa: E402
from readside.verifier import _derive_authenticated

LATE = 1000 + 10_000 + 1001            # ms: after the proof window and the grace period
CTX = decimal.Context(prec=500)
INF, NAN, FMAX = float('inf'), float('nan'), 1.7976931348623157e308


def entry(kind='REAL', tolerance=0.05):
    return dict(tv.ENTRY, type=kind, verify=dict(tv.ENTRY['verify'], tolerance=tolerance))


def derive(samples, **kwargs):
    """tv.derive, but an exception from the verifier is a failed assertion: it must answer whatever it is given."""
    try:
        return _derive_authenticated(kwargs.pop('request', tv.REQ), kwargs.pop('entry', tv.ENTRY),
                                     list(kwargs.pop('events', [tv.ACC])), list(samples),
                                     kwargs.pop('as_of', tv.AS_OF), kwargs.pop('sha', None))
    except Exception as exc:  # noqa: BLE001
        raise AssertionError('the verifier raised %r instead of answering' % (exc,)) from None


def run(kind, target, tolerance, values, as_of=tv.AS_OF, times=None):
    """derive() for one accepted action; one feedback sample per entry of `values`, in the proof window, in this order."""
    times = times or [6_000_000 + 100_000 * i for i in range(len(values))]
    samples = [tv.sample(t=t, value=v) for t, v in zip(times, values)]
    return derive(samples, events=[dict(tv.ACC, requested_value=target)], as_of=as_of, entry=entry(kind, tolerance))


def decoded(bits):
    return sampler.decode({'name': 'sp_flow_fb', 'type_name': 'REAL', 'bits': bits})


def oracle(target, got, tolerance):
    """|got - target| <= tolerance in decimal arithmetic; the decimal text of a float is what its JSON carried (repr)."""
    def dec(x):
        return decimal.Decimal(x) if type(x) is int else decimal.Decimal(repr(x))
    return abs(CTX.subtract(dec(got), dec(target))) <= dec(tolerance)


# (requested, feedback, tolerance, verified?)
REAL = [
    (12.5, 12.5, 0.05, True),
    (12.5, 12.55, 0.05, True),                  # exactly on the upper bound: inclusive
    (12.5, 12.45, 0.05, True),                  # exactly on the lower bound
    (12.5, 12.5500001, 0.05, False),
    (12.5, 12.4499999, 0.05, False),
    (12.5, 12.550000190734863, 0.05, False),    # what the f32 nearest to 12.55 decodes to: just outside
    (12.55, 12.550000190734863, 0.05, True),    # ... but it is 1.9e-7 from a REQUEST of 12.55
    (-12.5, -12.55, 0.05, True),
    (-12.5, -12.5500001, 0.05, False),
    (0.0, 0.0, 0.0, True),
    (0.0, 1e-10, 0.0, False),                   # the old allowance 1e-9 * max(1, |x|) accepted this
    (0.0, -1e-10, 0.0, False),
    (1e12, 1e12, 0.05, True),
    (1e12, 1e12 + 1000.0, 0.05, False),         # ... and 1000 at this magnitude
    (1e-30, 2e-30, 1e-30, True),                # tiny numbers, exact boundary
    (1e-30, 2.0000001e-30, 1e-30, False),
    (9007199254740993, 9007199254740992.0, 0.0, False),     # 2**53 + 1 against 2**53: float() makes them equal
    (FMAX, FMAX, 0.0, True),
    (FMAX, -FMAX, FMAX, False),                 # |difference| overflows a double
    (40, 40.0, 0, True),                        # an integer request, a float feedback
]
DINT = [
    (2000000000, 2000000000, 0, True),
    (2000000000, 2000000001, 0, False),         # F06: the old allowance was 1e-9 * 2e9 = 2
    (2000000000, 1999999999, 0, False),
    (2000000000.0, 2000000001, 0, False),       # the request may arrive as a float
    (2000000000, 2000000001, 1, True),
    (2000000000, 2000000002, 1, False),
    (2000000000, 2000000000.5, 0, False),       # a non-integer feedback is not truncated to the integer request
    (2000000000, 2000000000.0, 0, True),
    (-2147483648, -2147483648, 0, True),
    (-2147483648, -2147483647, 0, False),
    (-2147483648, -2147483647, 1, True),
    (2147483647, 2147483646, 0, False),
    (2147483647, 2147483647, 0, True),
    (0, 0, 0, True),
    (0, 1, 0, False),
    (1000, 1000, 0.5, True),
    (1000, 1001, 0.5, False),
]
INT = [
    (32767, 32767, 0, True),
    (32767, 32766, 0, False),
    (-32768, -32767, 0, False),
    (-32768, -32767, 1, True),
    (-32768, -32766, 1, False),
]
# (requested, feedback, verified?); the registry tolerance is irrelevant for BOOL
BOOL = [
    (True, True, True), (False, False, True), (True, False, False), (False, True, False),
    (1, True, True), (0, False, True), (1, 1, True), (0, 0, True), (1, 0, False), (0, 1, False),
    (1.0, True, True), (0.0, False, True), (1.0, 1, True),
    (True, 1.0, False),                         # a float is not a BOOL feedback
    (True, 2, False), (True, -1, False), (False, 2, False),
    (True, None, False), (True, 'true', False), (True, [], False), (True, NAN, False), (True, INF, False),
]
# the 32-bit patterns around the bounds of 12.5 +- 0.05, decoded by the lab sampler's own decoder
F32 = [
    (0x41480000, True), (0x4147ffff, True), (0x41480001, True),                    # 12.5 and its neighbours
    (0x4148cccc, True), (0x4148cccd, False), (0x4148ccce, False),                  # around 12.55: only the one below is inside
    (0x41473332, False), (0x41473333, False), (0x41473334, True),                  # around 12.45: only the one above is inside
]


class Tables(unittest.TestCase):
    def test_the_tables_agree_with_a_second_arithmetic_engine(self):
        for kind, rows in (('REAL', REAL), ('DINT', DINT), ('INT', INT)):
            for target, got, tol, expected in rows:
                with self.subTest(kind=kind, target=target, feedback=got, tolerance=tol):
                    self.assertEqual(oracle(target, got, tol), expected, 'the expected value in the table is wrong')
        for bits, expected in F32:
            with self.subTest(bits=hex(bits)):
                self.assertEqual(oracle(12.5, decoded(bits), 0.05), expected, 'the expected value in the table is wrong')

    def test_F06_numeric_types(self):
        for kind, rows in (('REAL', REAL), ('DINT', DINT), ('INT', INT)):
            for target, got, tol, expected in rows:
                with self.subTest(kind=kind, target=target, feedback=got, tolerance=tol):
                    r = run(kind, target, tol, [got])
                    self.assertEqual(r['status'] == 'verified', expected, r)
                    if expected:
                        self.assertEqual(r['observation_refs']['value'], got)

    def test_F06_real_f32_neighbours_of_the_bounds(self):
        for bits, expected in F32:
            with self.subTest(bits=hex(bits), value=decoded(bits)):
                s = tv.sample(t=6_000_000)
                s['variables'][0] = dict(s['variables'][0], bits=bits, decoded_value=decoded(bits))
                r = derive([s], events=[dict(tv.ACC, requested_value=12.5)], entry=entry('REAL', 0.05))
                self.assertEqual(r['status'] == 'verified', expected, r)

    def test_F06_bool_is_exact_whatever_the_tolerance(self):
        for tolerance in (0.0, 0.05, 1, 5, 1e9):
            for target, got, expected in BOOL:
                with self.subTest(tolerance=tolerance, target=target, feedback=got):
                    r = run('BOOL', target, tolerance, [got])
                    self.assertEqual(r['status'] == 'verified', expected, r)

    def test_F06_the_registry_decides_the_type_not_the_sample(self):
        # a sample that calls itself a BOOL, a REAL or a DINT changes nothing: only entry['type'] is read
        for claimed in ('BOOL', 'REAL', 'DINT', 'LREAL'):
            for registry_kind, target, got in (('DINT', 2000000000, 2000000001), ('DINT', 1, True), ('BOOL', 1, 2), ('REAL', 12.5, True)):
                with self.subTest(claimed=claimed, registry=registry_kind, target=target, feedback=got):
                    s = tv.sample(value=got)
                    s['variables'][0]['type_name'] = claimed
                    r = derive([s], events=[dict(tv.ACC, requested_value=target)], entry=entry(registry_kind, 0))
                    self.assertNotEqual(r['status'], 'verified', r)

    def test_a_registry_entry_without_a_usable_type_proves_nothing(self):
        typeless = {k: v for k, v in tv.ENTRY.items() if k != 'type'}
        for e in (typeless, dict(tv.ENTRY, type=None), dict(tv.ENTRY, type='LREAL'), dict(tv.ENTRY, type='real')):
            with self.subTest(type=e.get('type')):
                r = derive([tv.sample()], entry=e)
                self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'))
                self.assertIn('type', r.get('detail') or '')


class NonFinite(unittest.TestCase):
    def test_F05_non_finite_real_bit_patterns_never_prove_anything(self):
        for bits in (0x7F800000, 0xFF800000, 0x7FC00000, 0xFFC00000, 0x7F800001):
            value = decoded(bits)
            self.assertFalse(math.isfinite(value), 'precondition: %s is not finite' % hex(bits))
            for kind in ('REAL', 'DINT', 'INT'):
                with self.subTest(bits=hex(bits), kind=kind):
                    s = tv.sample(t=6_000_000)
                    s['variables'][0] = dict(s['variables'][0], bits=bits, decoded_value=value)
                    inside = derive([s], events=[dict(tv.ACC, requested_value=12 if kind != 'REAL' else 12.5)], entry=entry(kind, 0.05))
                    self.assertEqual(inside['status'], 'accepted', inside)
                    late = derive([s], events=[dict(tv.ACC, requested_value=12 if kind != 'REAL' else 12.5)], as_of=LATE, entry=entry(kind, 0.05))
                    self.assertEqual((late['status'], late['reason']), ('unknown', 'no_proof_by_deadline'), late)
                    self.assertTrue(late.get('detail'))

    def test_F05_the_legitimate_pattern_still_verifies(self):
        s = tv.sample(t=6_000_000)
        s['variables'][0] = dict(s['variables'][0], bits=0x41480000, decoded_value=decoded(0x41480000))
        r = derive([s], events=[dict(tv.ACC, requested_value=12.5)], entry=entry('REAL', 0.05))
        self.assertEqual(r['status'], 'verified')

    def test_F05_values_that_are_not_numbers_of_the_right_kind(self):
        # Requests of 0 and 1 are in the list on purpose: a bad value "repaired" into 0 or a bool read as 0/1 would match them.
        for kind, targets in (('REAL', (12.5, 0.0, 1.0)), ('DINT', (12, 0, 1)), ('INT', (12, 0, 1))):
            for target in targets:
                for bad in (NAN, INF, float('-inf'), None, 'x', '12.5', '0', [], {}, True, False, [12.5], {'v': 12.5}):
                    with self.subTest(kind=kind, request=target, feedback=bad):
                        r = run(kind, target, 0.05, [bad], as_of=LATE)
                        self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'), r)

    def test_F05_a_request_that_is_not_a_usable_number_is_unknown_at_once(self):
        # the feedback below equals the "request" wherever that is possible, so only the request's own validity can stop a proof
        bad_for_all = [NAN, INF, float('-inf'), None, '12.5', [], {}, [12.5]]
        cases = [('REAL', bad_for_all + [True]), ('DINT', bad_for_all + [12.5, True]), ('INT', bad_for_all + [12.5, 8.25]),
                 ('BOOL', bad_for_all + [2, -1, 0.5, 'true', '1'])]
        for kind, targets in cases:
            for target in targets:
                with self.subTest(kind=kind, request=target):
                    r = derive([tv.sample(value=target)], events=[dict(tv.ACC, requested_value=target)], entry=entry(kind, 0.05))
                    self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'), r)
                    self.assertTrue(r.get('detail'))

    def test_F05_a_tolerance_that_is_not_a_finite_number_at_or_above_zero(self):
        for bad in (-0.05, -1, NAN, INF, float('-inf'), None, '0.05', True, False, [], {}, [0.05]):
            for kind in ('REAL', 'DINT', 'INT'):
                with self.subTest(kind=kind, tolerance=bad):
                    r = run(kind, 12, bad, [12])           # the feedback equals the request: only the tolerance can stop the proof
                    self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'), r)
                    self.assertTrue(r.get('detail'))
        for good in (0, 0.0, -0.0, 1, 0.05):
            with self.subTest(tolerance=good):
                self.assertEqual(run('REAL', 12, good, [12])['status'], 'verified')

    def test_F05_a_bad_sample_does_not_hide_a_good_one_and_never_becomes_one(self):
        orders = {'bad then good': [INF, 12.5], 'good then bad': [12.5, INF], 'nan, -inf, good': [NAN, float('-inf'), 12.5],
                  'None then good': [None, 12.5], 'good, bad, bad': [12.5, NAN, INF]}
        for name, values in orders.items():
            with self.subTest(order=name):
                r = run('REAL', 12.5, 0.05, values)
                self.assertEqual(r['status'], 'verified', r)
                good = [i for i, v in enumerate(values) if v == 12.5][0]
                self.assertEqual(r['observation_refs']['scan_timestamp_us'], 6_000_000 + 100_000 * good)
                self.assertEqual(r['observation_refs']['value'], 12.5)
        for name, values in {'only bad': [INF], 'only bad, several': [INF, NAN, float('-inf'), None]}.items():
            with self.subTest(order=name):
                self.assertEqual(run('REAL', 12.5, 0.05, values)['status'], 'accepted')               # still inside the window
                r = run('REAL', 12.5, 0.05, values, as_of=LATE)
                self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'), r)
                self.assertIn(str(len(values)), r.get('detail') or '')
        with self.subTest(order='a good sample outside the window does not rescue a bad one inside it'):
            r = run('REAL', 12.5, 0.05, [INF, 12.5], times=[6_000_000, 10_000_001], as_of=LATE)
            self.assertEqual((r['status'], r['reason']), ('unknown', 'no_proof_by_deadline'), r)

    def test_F05_extreme_magnitudes_do_not_overflow_into_a_proof_or_an_exception(self):
        for target, got, tol in ((FMAX, -FMAX, FMAX), (-FMAX, FMAX, FMAX), (FMAX, -FMAX, 0.05), (10 ** 400, -10 ** 400, FMAX), (5e-324, -5e-324, 1e-324)):
            with self.subTest(target=target, feedback=got, tolerance=tol):
                self.assertNotEqual(run('REAL', target, tol, [got])['status'], 'verified')

    def test_unusable_samples_are_reported_in_detail_only_when_the_answer_is_unknown(self):
        waiting = run('REAL', 12.5, 0.05, [INF])
        self.assertEqual(waiting['status'], 'accepted')
        self.assertNotIn('detail', waiting)


class Unchanged(unittest.TestCase):
    """The behaviour P1 must not disturb, restated next to the new rules."""

    def test_the_inclusive_decimal_boundary_of_the_old_tests(self):
        self.assertEqual(run('REAL', 12.5, 0.05, [12.55])['status'], 'verified')
        self.assertNotEqual(run('REAL', 12.5, 0.05, [12.5500001])['status'], 'verified')

    def test_feedback_stale_is_still_reported_for_untrusted_inputs(self):
        s = tv.sample(stale=True, value=12.5)
        r = derive([s], events=[dict(tv.ACC, requested_value=12.5)], as_of=LATE, entry=entry('REAL', 0.05))
        self.assertEqual((r['status'], r['reason']), ('unknown', 'feedback_stale'))


if __name__ == '__main__':
    unittest.main()
