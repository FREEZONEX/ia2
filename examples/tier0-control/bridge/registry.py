"""The target registry (D9: JSON): human-reviewed, sha256-pinned, strictly parsed.  An entry may only name a target
the bridge can handle; anything the v0 bridge cannot honour (an approval requirement) makes it refuse to start."""
import hashlib
import math
import re

from . import strictjson

TYPES = ('REAL', 'DINT', 'INT', 'BOOL')
KEY_RE = re.compile(r'^[A-Za-z0-9_-]{1,64}$')
REQUIRED = ('key', 'device_id', 'ia2_variable', 'type', 'min', 'max', 'simulation', 'enabled')
OPTIONAL = {'requires_approval': False, 'require_devices': [], 'require_fieldbus_healthy': True,
            'max_rate_per_min': 6, 'min_interval_ms': 2000, 'max_deadline_ms': 60000, 'single_flight_ms': None, 'verify': None, 'note': ''}
VERIFY_FIELDS = ('feedback', 'tolerance', 'settle_ms')


class RegistryError(Exception):
    pass


class Registry:
    def __init__(self, targets, sha256):
        self.targets = targets
        self.sha256 = sha256


def _num(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def _check_entry(raw):
    if not isinstance(raw, dict):
        raise RegistryError('target must be an object')
    unknown = set(raw) - set(REQUIRED) - set(OPTIONAL)
    missing = [f for f in REQUIRED if f not in raw]
    if unknown:
        raise RegistryError('unknown target fields: %s' % sorted(unknown))
    if missing:
        raise RegistryError('missing target fields: %s' % missing)
    e = dict(OPTIONAL)
    e.update(raw)
    where = 'target %r' % e['key']
    if not isinstance(e['key'], str) or not KEY_RE.match(e['key']):
        raise RegistryError('bad key %r' % e['key'])
    for f in ('device_id', 'ia2_variable', 'note'):
        if not isinstance(e[f], str) or (f != 'note' and not e[f]):
            raise RegistryError('%s: %s must be a non-empty string' % (where, f))
    if e['type'] not in TYPES:
        raise RegistryError('%s: type must be one of %s' % (where, TYPES))
    if not (_num(e['min']) and _num(e['max']) and e['min'] <= e['max']):
        raise RegistryError('%s: min/max must be finite numbers with min <= max' % where)
    for f in ('simulation', 'enabled', 'requires_approval', 'require_fieldbus_healthy'):
        if not isinstance(e[f], bool):
            raise RegistryError('%s: %s must be a boolean' % (where, f))
    if not isinstance(e['require_devices'], list) or not all(isinstance(d, str) for d in e['require_devices']):
        raise RegistryError('%s: require_devices must be a list of strings' % where)
    for f in ('max_rate_per_min', 'min_interval_ms', 'max_deadline_ms'):
        if type(e[f]) is not int or e[f] < 0:
            raise RegistryError('%s: %s must be a non-negative integer' % (where, f))
    v = e['verify']
    if v is not None:
        if not isinstance(v, dict) or set(v) != set(VERIFY_FIELDS):
            raise RegistryError('%s: verify must have exactly %s' % (where, VERIFY_FIELDS))
        if not (isinstance(v['feedback'], str) and v['feedback'] and _num(v['tolerance']) and v['tolerance'] >= 0
                and type(v['settle_ms']) is int and v['settle_ms'] > 0):
            raise RegistryError('%s: bad verify values' % where)
    if e['single_flight_ms'] is None:        # how long an `accepted` action keeps its target busy: the read side has decided by then
        e['single_flight_ms'] = e['verify']['settle_ms'] if e['verify'] else 2000
    elif type(e['single_flight_ms']) is not int or e['single_flight_ms'] < 0:
        raise RegistryError('%s: single_flight_ms must be a non-negative integer' % where)
    # D3: v0 has no approval layer, so the reserved field must fail closed.
    if e['enabled'] and e['requires_approval']:
        raise RegistryError('%s: enabled with requires_approval=true, but v0 has no approval layer' % where)
    return e


def parse(text, sha256):
    try:
        doc = strictjson.loads(text)
    except strictjson.StrictJSONError as exc:
        raise RegistryError('registry is not strict JSON: %s' % exc) from None
    if not isinstance(doc, dict) or set(doc) != {'version', 'targets'} or doc['version'] != 1 \
            or not isinstance(doc['targets'], list):
        raise RegistryError('registry must be {"version":1,"targets":[...]}')
    targets = {}
    for raw in doc['targets']:
        e = _check_entry(raw)
        if e['key'] in targets:
            raise RegistryError('duplicate key %r' % e['key'])
        targets[e['key']] = e
    return Registry(targets, sha256)


def load_pinned(path, expected_sha256):
    with open(path, 'rb') as f:
        data = f.read()
    actual = hashlib.sha256(data).hexdigest()
    if actual != expected_sha256.lower():
        raise RegistryError('registry sha256 %s does not match the pinned %s' % (actual, expected_sha256))
    return parse(data.decode('utf-8'), actual)
