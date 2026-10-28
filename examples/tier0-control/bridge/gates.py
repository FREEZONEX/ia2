"""Gates G1 (shape) .. G9 as plain functions.  Each raises Reject or returns; none performs I/O except check_hold."""
import os
import re

from .errors import Reject
from . import runtime_schema

ACTION_ID = re.compile(r'^[0-9a-f]{32}$')
FIELD_TYPES = {'event_id': str, 'action_id': str, 'session_id': str, 'device_id': str, 'command': str,
               'target_key': str, 'simulation': bool, 'requested_at_ms': int, 'deadline_ms': int, 'requested_by': str}
MAX_SKEW_MS = 2000


def extract_ids(obj):
    """Whatever identifying fields a payload carries, for attributing a rejection (never trusted for anything else)."""
    ids = {'action_id': None, 'session_id': None, 'device_id': None, 'target_key': None}
    if isinstance(obj, dict):
        if isinstance(obj.get('action_id'), str) and ACTION_ID.match(obj['action_id']):
            ids['action_id'] = obj['action_id']
        for k in ('session_id', 'device_id', 'target_key'):
            if isinstance(obj.get(k), str):
                ids[k] = obj[k][:256]
    return ids


def validate_payload(obj):
    """G1 (shape).  Unknown or missing fields, wrong types, and any command but ia2_set are `bad_request`."""
    if not isinstance(obj, dict):
        raise Reject('bad_request', 'payload must be a JSON object')
    expected = set(FIELD_TYPES) | {'target'}
    if set(obj) != expected:
        extra, missing = sorted(set(obj) - expected), sorted(expected - set(obj))
        raise Reject('bad_request', 'payload fields differ (unknown=%s missing=%s)' % (extra, missing))
    for name, typ in FIELD_TYPES.items():
        v = obj[name]
        if type(v) is not typ:
            raise Reject('bad_request', 'field %s has the wrong type' % name)
        if typ is str and len(v) > 256:
            raise Reject('bad_request', 'field %s is too long' % name)
    if type(obj['target']) not in (bool, int, float):
        raise Reject('bad_request', 'target must be a number or boolean')
    if not ACTION_ID.match(obj['action_id']):
        raise Reject('bad_request', 'action_id must be 32 lowercase hex characters')
    if obj['command'] != 'ia2_set':
        raise Reject('bad_request', 'unsupported_command')
    return obj


def check_registry(payload, registry, disabled):
    """G2."""
    entry = registry.targets.get(payload['target_key'])
    if entry is None or not entry['enabled'] or entry['key'] in disabled:
        raise Reject('not_in_registry', 'no enabled registry entry for that key')
    if payload['device_id'] != entry['device_id']:
        raise Reject('not_in_registry', 'device_id does not match the registry entry')
    if payload['simulation'] != entry['simulation']:
        raise Reject('simulation_mismatch', 'request and registry disagree about simulation')
    return entry


def check_time(payload, entry, now_ms, arrived_ms=None):
    """G4.  The deadline is judged at processing time; the clock-skew limit against the moment the line ARRIVED, so a request that
    waited in the bridge's own queue is not mistaken for a stale one."""
    arrived_ms = now_ms if arrived_ms is None else arrived_ms
    if payload['deadline_ms'] <= now_ms:
        raise Reject('expired', 'deadline is not in the future')
    if payload['deadline_ms'] - now_ms > entry['max_deadline_ms']:
        raise Reject('expired', 'deadline is further away than max_deadline_ms')
    if abs(payload['requested_at_ms'] - arrived_ms) > MAX_SKEW_MS:
        raise Reject('expired', 'requested_at_ms is more than 2 s from the bridge clock')


def check_hold(path):
    """G7: the stop switch refuses new requests; it never undoes a write already sent."""
    if path and os.path.exists(path):
        raise Reject('held', 'hold file present')


def _forced(forces, variable):
    short = variable.split('.')[-1].lower()
    for item in forces or []:
        name = item.get('name') or item.get('variable') if isinstance(item, dict) else item
        if not isinstance(name, str):
            return True            # cannot read the entry: fail closed
        if name.split('.')[-1].lower() == short:
            return True
    return False


def check_runtime(health, status_a, status_b, skip_g8=False):
    """G8 on two /status reads and one /health read.  Returns the runtime identity."""
    runtime_schema.health(health)
    runtime_schema.status(status_a)
    runtime_schema.status(status_b)
    runtime_id = status_b['runtime_id']
    if status_a['runtime_id'] != runtime_id:
        raise Reject('runtime_unreachable', 'runtime identity changed between the two reads')
    if skip_g8:
        return runtime_id
    if health.get('status') != 'ok':
        raise Reject('runtime_unreachable', 'health.status is not ok')
    if health.get('fault') is not None or status_b.get('fault') is not None:
        raise Reject('vm_error', 'the runtime reports a fault')
    if health.get('watchdog_tripped') is not False or status_b.get('watchdog_tripped') is not False \
            or health.get('latch_reason') is not None or status_b.get('latch_reason') is not None:
        raise Reject('latched', 'outputs are latched off')
    if runtime_schema.failsafe_active(health, status_a, status_b):
        raise Reject('latched', 'failsafe_status is not clear')
    tap = health.get('snapshot_tap')
    if not isinstance(tap, dict) or tap.get('running') is not True:
        raise Reject('runtime_unreachable', 'snapshot task is not running')
    a, b = status_a.get('scan_count'), status_b.get('scan_count')
    if type(a) is not int or type(b) is not int or b <= a:
        raise Reject('runtime_unreachable', 'scan_count did not advance between the reads')
    return runtime_id


def check_target(health, status, entry):
    """G9."""
    runtime_schema.status(status, entry=entry)
    runtime_schema.health(health, entry['require_fieldbus_healthy'])
    mode = status.get('mode')
    if not isinstance(mode, dict) or mode.get('kind') != 'running':
        raise Reject('paused', 'the runtime is not in continuous running mode')
    if _forced(status.get('forces'), entry['ia2_variable']):
        raise Reject('forced', 'the target (or an unreadable force entry) is forced')
    healthy = {d.get('name'): d.get('healthy') for d in status.get('device_health', []) if isinstance(d, dict)}
    for dev in entry['require_devices']:
        if healthy.get(dev) is not True:
            raise Reject('device_unhealthy', 'required device %s is not healthy' % dev)
    if entry['require_fieldbus_healthy'] and health.get('fieldbus_healthy') is not True:
        raise Reject('fieldbus_unhealthy', 'fieldbus_healthy is not true')
