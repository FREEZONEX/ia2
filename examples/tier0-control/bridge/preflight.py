"""Startup probe of the live runtime (W15).  The bridge needs three things from /status that old builds lack: a `runtime_id` (the
restart detector), and, for every registry target with a feedback channel, that variable present in the snapshot with an `input`
block carrying `stale`.  Better to say so at start than to refuse every request later.  A runtime that cannot be reached yet is
not an error (start order is not the bridge's business): the same facts are checked again on every request (G8)."""

from . import runtime_schema
from .errors import Reject


def check(client, registry):
    status, rec = client.get_json('/status')
    if status is None:
        return {'reachable': False, 'problems': [], 'runtime_id': None, 'build_commit': None}
    problems = []
    try:
        runtime_schema.status(status, 'startup')
        enabled = [t for t in registry.targets.values() if t['enabled']]
        for t in enabled:
            runtime_schema.status(status, 'startup', t)
        health, _ = client.get_json('/health')
        runtime_schema.health(health, any(t['require_fieldbus_healthy'] for t in enabled))
    except Reject as exc:
        detail = exc.detail
        if 'runtime_id' in detail:
            detail += '; legacy identity is NOT supported: deploy a runtime that has it'
        return {'reachable': True, 'problems': [detail], 'runtime_id': None, 'build_commit': None}
    snap = status.get('last_snapshot')
    rows = {v.get('name'): v for v in snap.get('vars', [])} if isinstance(snap, dict) else {}
    for t in registry.targets.values():
        if not t['enabled']:
            continue
        short = t['ia2_variable'].split('.')[-1]
        if short not in rows:
            problems.append('target %s: variable %s is not in the runtime snapshot' % (t['key'], t['ia2_variable']))
        if t['verify']:
            row = rows.get(t['verify']['feedback'])
            if row is None:
                problems.append('target %s: feedback variable %s is not in the runtime snapshot' % (t['key'], t['verify']['feedback']))
            elif not isinstance(row.get('input'), dict) or type(row['input'].get('stale')) is not bool:
                problems.append('target %s: feedback variable %s has no input.stale (it is not an Input-mapped variable, or the runtime '
                                'does not report staleness)' % (t['key'], t['verify']['feedback']))
    return {'reachable': True, 'problems': problems, 'runtime_id': status.get('runtime_id'), 'build_commit': status.get('build_commit')}
