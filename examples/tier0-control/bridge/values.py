"""Value checks and the bit encoding /write expects (an i32; REAL is the f32 bit pattern)."""
import math
import struct

from .errors import Reject


def _f32(x):
    try:
        return struct.unpack('>f', struct.pack('>f', x))[0]
    except OverflowError:
        raise Reject('out_of_range', 'does not fit a REAL') from None


def f32_bits(x):
    return struct.unpack('>i', struct.pack('>f', x))[0]


def bits_to_f32(bits):
    return struct.unpack('>f', struct.pack('>i', bits))[0]


def decode(type_name, bits):
    return bits_to_f32(bits) if type_name == 'REAL' else bits


def validate_and_encode(entry, target):
    """Return (bits, expected_value).  Raises Reject(bad_value | out_of_range | unsupported_type).

    Deliberately stricter than IA2 (WP0 measured): IA2 accepts BOOL=2, clamps +inf to max and only refuses NaN."""
    kind = entry['type']
    lo, hi = entry['min'], entry['max']
    if kind == 'BOOL':
        if type(target) is float and target == 0 and math.copysign(1.0, target) < 0:
            raise Reject('bad_value', 'negative zero')
        if isinstance(target, bool):
            v = int(target)
        elif type(target) is int and target in (0, 1):
            v = target
        elif type(target) is float and target in (0.0, 1.0):
            v = int(target)
        else:
            raise Reject('bad_value', 'BOOL accepts true/false/0/1 only')
        if not lo <= v <= hi:
            raise Reject('out_of_range', 'outside the registry range')
        return v, v
    if isinstance(target, bool) or not isinstance(target, (int, float)):
        raise Reject('bad_value', 'target must be a number')
    try:
        x = float(target)
    except OverflowError:                       # an int beyond the float range (JSON allows any size)
        raise Reject('out_of_range', 'integer too large for a number') from None
    if not math.isfinite(x):
        raise Reject('bad_value', 'target is not finite')
    if x == 0 and math.copysign(1.0, x) < 0:
        raise Reject('bad_value', 'negative zero')
    if not lo <= x <= hi:
        raise Reject('out_of_range', 'outside the registry range')
    if kind == 'REAL':
        y = _f32(x)
        if not lo <= y <= hi:
            raise Reject('out_of_range', 'outside the registry range after rounding to a 32-bit REAL')
        return f32_bits(y), y
    if x != math.floor(x):
        raise Reject('bad_value', '%s needs an integral value' % kind)
    i = int(x)
    width = (-32768, 32767) if kind == 'INT' else (-2 ** 31, 2 ** 31 - 1)
    if not width[0] <= i <= width[1]:
        raise Reject('out_of_range', 'does not fit %s' % kind)
    return i, i
