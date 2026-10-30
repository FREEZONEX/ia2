"""Explicit signing adapters for legacy semantic vectors; never shipped in prod.

D11 negative tests call the production verifier directly, without these adapters.
"""
import hashlib
import hmac
import json
import os
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bridge.trace import TraceWriter as RealTraceWriter
from bridge import eventauth
from readside import auth
from readside.verifier import derive as signed_derive, _exact, _requested_number, _feedback_number

EVENT_KEYS = {'event-test': dict(secret=b'E' * 32, not_before_ms=0, not_after_ms=2**60)}
SAMPLE_KEYS = {'sample-test': dict(secret=b'S' * 32, not_before_ms=0, not_after_ms=2**60)}


def event_record(record):
    r = dict(record)
    r.setdefault('schema', 'ia2.control/v0')
    r.setdefault('kind', 'action_status' if 'action_id' in r else 'bridge_notice')
    r.setdefault('observed_at_ms', 0)
    r.setdefault('event_id', hashlib.sha256(json.dumps(r, sort_keys=True).encode()).hexdigest()[:32])
    return r


def sample_record(record):
    r = dict(record)
    r.setdefault('schema', 'ia2.sample/lab')
    r.setdefault('collected_at_ms', r.get('observed_at_ms', 0))
    return r


def _fixture_sign(record, keys, role):
    kid, key = next(iter(keys.items()))
    version = auth.VERSIONS[role]
    text = json.dumps(record, sort_keys=True, separators=(',', ':'), ensure_ascii=True)
    return dict(v=version, key_id=kid, payload=text,
                sig=hmac.new(key['secret'], (version + '\n' + text).encode(), hashlib.sha256).hexdigest())


def event(record):
    return _fixture_sign(event_record(record), EVENT_KEYS, 'event')


def sample(record):
    return _fixture_sign(sample_record(record), SAMPLE_KEYS, 'sample')


def derive(request, entry, events, samples, as_of_ms, registry_sha256=None):
    return signed_derive(request, entry, [event(e) for e in events], [sample(s) for s in samples], as_of_ms,
                         registry_sha256=registry_sha256, event_keys=EVENT_KEYS, sample_keys=SAMPLE_KEYS)


class TraceWriter(RealTraceWriter):
    def __init__(self, path):
        super().__init__(path, EVENT_KEYS)

    def emit(self, record, anchor=None):
        return super().emit(event_record(record), anchor)


def decode_event(record):
    return auth.verify(record, EVENT_KEYS, 'event')


def event_keys_path(request_path):
    p = str(request_path) + '.events'
    if Path(request_path).exists():
        Path(p).write_text(json.dumps({k: dict(secret_hex=v['secret'].hex(), not_before_ms=v['not_before_ms'],
                                            not_after_ms=v['not_after_ms']) for k, v in EVENT_KEYS.items()}))
        os.chmod(p, 0o600)
    return p
