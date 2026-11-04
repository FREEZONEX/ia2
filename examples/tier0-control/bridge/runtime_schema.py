"""Phase-specific evidence shapes. Missing safety fields never imply a safe value.

Older runtimes may omit failsafe_status; current runtimes return null before any
attempt. A non-null report must contain exactly three lists. No report is not a
safety assertion: all other required fields and each snapshot are still checked.
"""
from .errors import Reject


def fail(path):
    raise Reject('runtime_unreachable', 'missing or invalid runtime evidence: ' + path)


def obj(value, path):
    if not isinstance(value, dict):
        fail(path)
    return value


def field(doc, key, typ, path):
    if key not in doc or type(doc[key]) is not typ:
        fail(path + '.' + key)
    return doc[key]


def text(doc, key, path):
    value = field(doc, key, str, path)
    if not value:
        fail(path + '.' + key)
    return value


def flags(doc, path):
    field(doc, 'watchdog_tripped', bool, path)
    for key in ('fault', 'latch_reason'):
        if key not in doc or (doc[key] is not None and type(doc[key]) not in (str, dict)):
            fail(path + '.' + key)
    if doc.get('failsafe_status') is not None:
        fs = obj(doc['failsafe_status'], path + '.failsafe_status')
        if set(fs) != {'pending', 'acknowledged', 'unconfirmed'}:
            fail(path + '.failsafe_status')
        for key in ('pending', 'acknowledged', 'unconfirmed'):
            field(fs, key, list, path + '.failsafe_status')


def identity(doc):
    obj(doc, 'status')
    return text(doc, 'runtime_id', 'status')


def status(doc, phase='pre', entry=None):
    identity(doc)
    flags(doc, 'status')
    if field(doc, 'scan_count', int, 'status') < 0:
        fail('status.scan_count')
    if phase in ('pre', 'startup'):
        mode = obj(doc.get('mode'), 'status.mode')
        text(mode, 'kind', 'status.mode')
        forces = field(doc, 'forces', list, 'status')
        for item in forces:
            name = item.get('name', item.get('variable')) if isinstance(item, dict) else item
            if type(name) is not str or not name:
                fail('status.forces[]')
        if entry and entry['require_devices']:
            devices = field(doc, 'device_health', list, 'status')
            names = set()
            for dev in devices:
                obj(dev, 'status.device_health[]')
                name = text(dev, 'name', 'status.device_health[]')
                field(dev, 'healthy', bool, 'status.device_health[]')
                if name in names:
                    fail('status.device_health[].name (duplicate)')
                names.add(name)
    if phase in ('post', 'recovery', 'startup'):
        snap = obj(doc.get('last_snapshot'), 'status.last_snapshot')
        if field(snap, 'timestamp_us', int, 'status.last_snapshot') < 0:
            fail('status.last_snapshot.timestamp_us')
        if phase == 'startup':
            for row in field(snap, 'vars', list, 'status.last_snapshot'):
                obj(row, 'status.last_snapshot.vars[]')
                text(row, 'name', 'status.last_snapshot.vars[]')


def health(doc, require_fieldbus=False):
    obj(doc, 'health')
    text(doc, 'status', 'health')
    flags(doc, 'health')
    tap = obj(doc.get('snapshot_tap'), 'health.snapshot_tap')
    field(tap, 'running', bool, 'health.snapshot_tap')
    if require_fieldbus:
        field(doc, 'fieldbus_healthy', bool, 'health')


def failsafe_active(*docs):
    # Call after shape validation. Missing/null means no report, not a cleared latch.
    return any(any(doc['failsafe_status'].values()) for doc in docs if doc.get('failsafe_status') is not None)


def unsafe_after(status_doc, health_doc):
    return (health_doc['status'] != 'ok' or not health_doc['snapshot_tap']['running']
            or any(d['fault'] is not None or d['watchdog_tripped'] or d['latch_reason'] is not None
                   for d in (status_doc, health_doc))
            or failsafe_active(status_doc, health_doc))
