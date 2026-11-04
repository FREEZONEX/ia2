"""D11 evidence envelopes. No bridge imports; only contract vocabulary is shared.

Keys authenticate the speaker, not the truth of its report. HMAC verifiers hold
secrets and can therefore forge evidence. Neither keys nor rejected payloads are
returned in diagnostics.
"""
import hashlib
import hmac
import json
import math
import os
import re
import stat

with open(os.path.join(os.path.dirname(__file__), '..', 'contract_vocab.json'), encoding='utf-8') as _f:
    VOCAB = json.load(_f)
VERSIONS = VOCAB['evidence_versions']
KINDS = frozenset(('action_status', 'conflict_notice', 'reconcile_note', 'bridge_notice'))
MAX_ENVELOPE = 4 * 1024 * 1024


def _pairs(items):
    out = {}
    for key, value in items:
        if key in out:
            raise ValueError('duplicate JSON field')
        out[key] = value
    return out


def _bad_constant(value):
    raise ValueError('nonfinite JSON number')


def _finite(value):
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError('nonfinite JSON number')
    # JSON escapes in diagnostic strings are preserved, including a lone
    # surrogate from a rejected request. UTF-8 is enforced on wire bytes and
    # on the exact signed payload, not by re-encoding decoded string values.
    if isinstance(value, dict):
        for key, item in value.items():
            _finite(key)
            _finite(item)
    elif isinstance(value, list):
        for item in value:
            _finite(item)


def loads(raw):
    try:
        if len(raw) > MAX_ENVELOPE:
            raise ValueError('JSON exceeds size limit')
        if isinstance(raw, (bytes, bytearray)):
            raw = raw.decode('utf-8')  # json.loads(bytes) also accepts UTF-16/32
        value = json.loads(raw, object_pairs_hook=_pairs, parse_constant=_bad_constant)
        _finite(value)
        return value
    except (ValueError, TypeError, RecursionError, UnicodeError):
        raise ValueError('invalid strict JSON') from None


def load_keys(path):
    with open(path, encoding='utf-8') as f:
        info = os.fstat(f.fileno())
        if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600:
            raise ValueError('key file must be a regular mode-0600 file')
        obj = loads(f.read(MAX_ENVELOPE + 1))
    if not isinstance(obj, dict) or not obj:
        raise ValueError('empty or invalid key ring')
    out = {}
    for kid, spec in obj.items():
        if not isinstance(kid, str) or not re.fullmatch(r'[A-Za-z0-9._-]{1,64}', kid) or not isinstance(spec, dict):
            raise ValueError('invalid key entry')
        if set(spec) != {'secret_hex', 'not_before_ms', 'not_after_ms'}:
            raise ValueError('invalid key fields')
        secret = spec['secret_hex']
        lo, hi = spec['not_before_ms'], spec['not_after_ms']
        if not isinstance(secret, str) or not re.fullmatch(r'[0-9a-fA-F]{64,128}', secret) or len(secret) % 2 \
                or type(lo) is not int or type(hi) is not int or not 0 <= lo <= hi:
            raise ValueError('invalid key material or validity window')
        out[kid] = dict(secret=bytes.fromhex(secret), not_before_ms=lo, not_after_ms=hi)
    return out


def separate_keys(*rings):
    seen = set()
    for ring in rings:
        current = {key['secret'] for key in ring.values()}
        if not current or seen.intersection(current):
            raise ValueError('each role needs a separate nonempty key ring')
        seen.update(current)


def load_revoked(path):
    if path is None:
        return frozenset()
    with open(path, encoding='utf-8') as f:
        value = loads(f.read(MAX_ENVELOPE + 1))
    if not isinstance(value, list) or not all(isinstance(k, str) for k in value):
        raise ValueError('revocations must be a JSON array of key IDs')
    return frozenset(value)


def select_key(keys, timestamp, revoked=()):
    valid = [(key['not_before_ms'], kid, key) for kid, key in keys.items()
             if kid not in revoked and key['not_before_ms'] <= timestamp <= key['not_after_ms']]
    if not valid:
        raise ValueError('no valid signing key')
    _, kid, key = max(valid, key=lambda item: item[:2])
    return kid, key


def _stamp(record, role):
    return record.get('observed_at_ms' if role == 'event' else 'collected_at_ms')


def _hint(record, role):
    names = ('kind', 'action_id', 'event_id') if role == 'event' else ('event_id', 'native_runtime_id')
    return {name: record[name] for name in names if name in record}


def _shape(record, role):
    if not isinstance(record, dict) or type(_stamp(record, role)) is not int or _stamp(record, role) < 0 \
            or not isinstance(record.get('event_id'), str) or not record['event_id']:
        raise ValueError('invalid evidence identity or timestamp')
    if role == 'event':
        if record.get('schema') != 'ia2.control/v0' or record.get('kind') not in KINDS:
            raise ValueError('invalid event schema or kind')
        if record['kind'] != 'bridge_notice' and not isinstance(record.get('action_id'), str):
            raise ValueError('missing action identity')
    else:
        if record.get('schema') != 'ia2.sample/lab' or not isinstance(record.get('native_runtime_id'), str):
            raise ValueError('invalid sample schema or runtime')
        for name in ('scan_count', 'scan_timestamp_us'):
            if type(record.get(name)) is not int or record[name] < 0:
                raise ValueError('invalid scan marker')
        for name in ('variables', 'device_health'):
            if not isinstance(record.get(name), list) or not all(isinstance(v, dict) for v in record[name]):
                raise ValueError('invalid sample collection')
        if not isinstance(record.get('forces'), list):
            raise ValueError('invalid forces')


def sign(record, keys, role, revoked=()):
    _shape(record, role)
    kid, key = select_key(keys, _stamp(record, role), revoked)
    payload = json.dumps(record, sort_keys=True, separators=(',', ':'), ensure_ascii=True, allow_nan=False)
    version = VERSIONS[role]
    signature = hmac.new(key['secret'], (version + '\n' + payload).encode('utf-8'), hashlib.sha256).hexdigest()
    envelope = dict(v=version, key_id=kid, payload=payload, sig=signature, hint=_hint(record, role))
    if len(json.dumps(envelope)) > MAX_ENVELOPE:
        raise ValueError('evidence exceeds size limit')
    return envelope


def verify(raw, keys, role, revoked=()):
    try:
        env = loads(raw) if isinstance(raw, (str, bytes)) else raw
        required = {'v', 'key_id', 'payload', 'sig'}
        if not isinstance(env, dict) or not required <= set(env) <= required | {'hint'}:
            raise ValueError('invalid envelope fields')
        if env['v'] != VERSIONS[role] or not all(isinstance(env[k], str) for k in ('key_id', 'payload', 'sig')):
            raise ValueError('invalid envelope types or role')
        if len(env['payload']) > MAX_ENVELOPE or not re.fullmatch('[0-9a-f]{64}', env['sig']):
            raise ValueError('invalid signature or size')
        key = keys.get(env['key_id'])
        if key is None or env['key_id'] in revoked:
            raise ValueError('unknown or revoked key')
        expected = hmac.new(key['secret'], (env['v'] + '\n' + env['payload']).encode('utf-8'), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(expected, env['sig']):
            raise ValueError('invalid signature')
        record = loads(env['payload'])  # only after authenticating the exact bytes
        _shape(record, role)
        if not key['not_before_ms'] <= _stamp(record, role) <= key['not_after_ms']:
            raise ValueError('evidence outside key validity window')
        if 'hint' in env and env['hint'] != _hint(record, role):
            raise ValueError('hint disagrees with authenticated payload')
        return record
    except (ValueError, TypeError, KeyError, RecursionError, UnicodeError):
        raise ValueError('rejected evidence envelope') from None


def verified_records(envelopes, keys, role, revoked=()):
    out, rejected = [], 0
    for envelope in envelopes:
        try:
            out.append(verify(envelope, keys, role, revoked))
        except ValueError:
            rejected += 1
    return out, rejected
