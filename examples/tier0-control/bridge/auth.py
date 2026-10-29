"""Request envelope and HMAC check (D2, D10): the signature covers the exact bytes of `payload`, and the payload
is parsed only after it verifies."""
import hashlib
import hmac
import os
import re

from . import strictjson
from .errors import Reject

PREFIX = b'ia2.control/v0\n'
ENVELOPE_FIELDS = frozenset(('v', 'key_id', 'payload', 'sig'))
HEX64 = re.compile(r'^[0-9a-f]{64}$')


def sign(secret, payload_text):
    return hmac.new(secret, PREFIX + payload_text.encode('utf-8'), hashlib.sha256).hexdigest()


def load_keys(path):
    if os.stat(path).st_mode & 0o077:
        raise ValueError('keys file %s is readable by group or others; chmod 600 it' % path)
    with open(path, encoding='utf-8') as f:
        obj = strictjson.loads(f.read())
    keys = {}
    for key_id, spec in obj.items():
        keys[key_id] = {'secret': bytes.fromhex(spec['secret_hex']),
                        'not_before_ms': int(spec.get('not_before_ms', 0)),
                        'not_after_ms': int(spec['not_after_ms'])}
    return keys


def verify_envelope(raw_line, keys, now_ms):
    """Return (payload_text, key_id) or raise Reject('bad_request' | 'unauthenticated').  Details never contain key
    material or the signature."""
    try:
        env = strictjson.loads(raw_line)
    except strictjson.StrictJSONError as exc:
        raise Reject('bad_request', 'envelope is not strict JSON: %s' % exc) from None
    if not isinstance(env, dict) or set(env) != ENVELOPE_FIELDS:
        raise Reject('bad_request', 'envelope must have exactly the fields v, key_id, payload, sig')
    if env['v'] != 'ia2.control/v0' or not all(isinstance(env[k], str) for k in ('key_id', 'payload', 'sig')):
        raise Reject('bad_request', 'envelope version or field types')
    key = keys.get(env['key_id'])
    if key is None:
        raise Reject('unauthenticated', 'unknown key_id')
    if not key['not_before_ms'] <= now_ms <= key['not_after_ms']:
        raise Reject('unauthenticated', 'key outside its validity window')
    try:
        expected = sign(key['secret'], env['payload'])
    except UnicodeEncodeError:
        raise Reject('bad_request', 'payload is not encodable as UTF-8') from None
    if not HEX64.match(env['sig']) or not hmac.compare_digest(expected, env['sig']):
        raise Reject('unauthenticated', 'bad signature')
    return env['payload'], env['key_id']
