"""Bridge-side event signing and recovery authentication, independent of readside.

The read-side verifier intentionally does not import this implementation.
"""
import hashlib
import hmac
import json
import os
import re
import stat

from . import strictjson

with open(os.path.join(os.path.dirname(__file__), '..', 'contract_vocab.json'), encoding='utf-8') as _f:
    VERSION = json.load(_f)['evidence_versions']['event']
KINDS = ('action_status', 'conflict_notice', 'reconcile_note', 'bridge_notice')


def load_keys(path):
    with open(path, encoding='utf-8') as f:
        info = os.fstat(f.fileno())
        if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600:
            raise ValueError('event keys require a regular mode-0600 file')
        text = f.read(65537)
    if len(text) > 65536:
        raise ValueError('event key file exceeds size limit')
    obj = strictjson.loads(text)
    if not isinstance(obj, dict) or not obj:
        raise ValueError('invalid event key ring')
    keys = {}
    for kid, spec in obj.items():
        if not isinstance(kid, str) or not re.fullmatch(r'[A-Za-z0-9._-]{1,64}', kid) or not isinstance(spec, dict) \
                or set(spec) != {'secret_hex', 'not_before_ms', 'not_after_ms'}:
            raise ValueError('invalid event key entry')
        secret, lo, hi = spec['secret_hex'], spec['not_before_ms'], spec['not_after_ms']
        if not isinstance(secret, str) or not re.fullmatch('[0-9a-fA-F]{64,128}', secret) or len(secret) % 2 \
                or type(lo) is not int or type(hi) is not int or not 0 <= lo <= hi:
            raise ValueError('invalid event key material or window')
        keys[kid] = dict(secret=bytes.fromhex(secret), not_before_ms=lo, not_after_ms=hi)
    return keys


def _check(event):
    if not isinstance(event, dict) or event.get('schema') != 'ia2.control/v0' or event.get('kind') not in KINDS \
            or not isinstance(event.get('event_id'), str) or not event['event_id'] \
            or type(event.get('observed_at_ms')) is not int or event['observed_at_ms'] < 0:
        raise ValueError('invalid bridge event')
    if event['kind'] != 'bridge_notice' and not isinstance(event.get('action_id'), str):
        raise ValueError('missing action identity')
    # The stdlib parser accepts exponent overflow. Recovery must reject it
    # just like the read side. Preserve JSON escapes in diagnostic strings;
    # this validation never changes the payload bytes covered by the MAC.
    json.dumps(event, ensure_ascii=True, allow_nan=False)


def _hint(event):
    return {k: event[k] for k in ('kind', 'action_id', 'event_id') if k in event}


def sign(event, keys, revoked=()):
    _check(event)
    valid = [(k['not_before_ms'], kid, k) for kid, k in keys.items() if kid not in revoked
             and k['not_before_ms'] <= event['observed_at_ms'] <= k['not_after_ms']]
    if not valid:
        raise ValueError('no valid event signing key')
    _, kid, key = max(valid, key=lambda row: row[:2])
    payload = json.dumps(event, sort_keys=True, separators=(',', ':'), ensure_ascii=True, allow_nan=False)
    sig = hmac.new(key['secret'], (VERSION + '\n' + payload).encode('utf-8'), hashlib.sha256).hexdigest()
    return dict(v=VERSION, key_id=kid, payload=payload, sig=sig, hint=_hint(event))


def verify(raw, keys, revoked=()):
    try:
        env = strictjson.loads(raw)
        required = {'v', 'key_id', 'payload', 'sig'}
        if not isinstance(env, dict) or not required <= set(env) <= required | {'hint'} or env['v'] != VERSION \
                or not all(isinstance(env[k], str) for k in ('key_id', 'payload', 'sig')):
            raise ValueError('invalid event envelope')
        key = keys.get(env['key_id'])
        if key is None or env['key_id'] in revoked or not re.fullmatch('[0-9a-f]{64}', env['sig']):
            raise ValueError('unknown key or invalid signature')
        expected = hmac.new(key['secret'], (VERSION + '\n' + env['payload']).encode('utf-8'), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(expected, env['sig']):
            raise ValueError('invalid signature')
        event = strictjson.loads(env['payload'])
        _check(event)
        if not key['not_before_ms'] <= event['observed_at_ms'] <= key['not_after_ms']:
            raise ValueError('event outside key window')
        if 'hint' in env and env['hint'] != _hint(event):
            raise ValueError('inconsistent hint')
        return event
    except (ValueError, TypeError, KeyError, RecursionError, UnicodeError):
        raise ValueError('untrusted trace record') from None
