"""ia2-control-bridge: tail a requests file, run each line through the engine, write the journal and trace.

Exit codes: 2 = registry / keys / journal problem (or an unreadable keys file, or a status file that cannot be written), 3 = northbound
self-check refused (D6), 4 = the live runtime lacks what the bridge needs (preflight), 5 = the registry leaves the project's governance,
6 = the journal is shorter than, or disagrees with, the anchor the trace holds, 7 = while running, the journal could not be written or
synced: the bridge stops so that nothing more is sent, and the next start replays what is on disk.  On start it prints one JSON line
{"ready": true, ...} to stdout."""
import argparse
import json
import os
import sys
import time

from . import auth, build_flavor, eventauth, governance_check, northbound_guard, preflight, strictjson
from . import registry as registry_mod
from .engine import Engine, now_ms
from .errors import Reject
from .ia2client import Ia2Client
from .journal import Journal, JournalError, JournalWriteError
from .tail import RequestTail
from .trace import TraceWriter


def write_status(path, snapshot):
    tmp = path + '.tmp'
    with open(tmp, 'w', encoding='ascii') as f:
        json.dump(snapshot, f, sort_keys=True)
    os.replace(tmp, path)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--registry', required=True)
    ap.add_argument('--registry-sha256', required=True)
    ap.add_argument('--keys', required=True, help='JSON key file; must not be readable by group or others')
    ap.add_argument('--event-keys', required=True, help='separate mode-0600 event signing keys')
    ap.add_argument('--revoked-keys', help='JSON array of revoked event key IDs; reload on restart')
    ap.add_argument('--runtime', required=True, help='host:port of the IA2 edge runtime (127.0.0.1 only)')
    ap.add_argument('--journal', required=True)
    ap.add_argument('--requests', required=True)
    ap.add_argument('--trace', required=True)
    ap.add_argument('--project-dir', required=True, help='deployed project dir: northbound.toml (D6) and the [governance] table are read from it')
    ap.add_argument('--hold-file', default=None)
    ap.add_argument('--enable-file', default=None, help='operator re-enable lines for disabled targets (default: <journal>.enable)')
    ap.add_argument('--status-file', default=None, help='counters for an operator (default: <journal>.status.json)')
    ap.add_argument('--poll-ms', type=int, default=100)
    ap.add_argument('--advance-wait-ms', type=int, default=600, help='how long G8 waits for the published scan_count to advance')
    ap.add_argument('--reconcile-recheck-ms', type=int, default=10000, help='pause before the second /audit look at an unknown write')
    ap.add_argument('--max-line-bytes', type=int, default=65536)
    ap.add_argument('--governance-check', choices=('strict', 'warn', 'off'), default='strict',
                    help='registry ranges must lie inside the project governance: strict refuses to start, warn records it')
    ap.add_argument('--preflight', choices=('on', 'off'), default='on')
    # --- lab-only begin
    ap.add_argument('--test-hooks', default=None, help='lab build only: a module file defining HOOKS')
    # --- lab-only end
    args = ap.parse_args(argv)

    host, _, port = args.runtime.partition(':')
    if host not in ('127.0.0.1', 'localhost'):
        print('refusing a non-loopback runtime address', file=sys.stderr)
        return 2
    try:
        keys = auth.load_keys(args.keys)
        event_keys = eventauth.load_keys(args.event_keys)
        if {k['secret'] for k in keys.values()} & {k['secret'] for k in event_keys.values()}:
            raise ValueError('request and event keys must be independent')
        revoked = []
        if args.revoked_keys:
            with open(args.revoked_keys, encoding='utf-8') as f:
                revoked = strictjson.loads(f.read())
            if not isinstance(revoked, list) or not all(isinstance(k, str) for k in revoked):
                raise ValueError('invalid revocation list')
        reg = registry_mod.load_pinned(args.registry, args.registry_sha256)
    except (registry_mod.RegistryError, OSError, ValueError, KeyError) as exc:
        print('startup refused: %s' % exc, file=sys.stderr)
        return 2
    try:
        nb = northbound_guard.check(args.project_dir)
    except Reject as exc:
        print('startup refused: %s' % exc, file=sys.stderr)
        return 3
    findings = []
    if args.governance_check != 'off':
        try:
            findings = governance_check.check(reg.targets.values(), args.project_dir)
        except OSError as exc:
            print('startup refused: cannot read the project governance (%s); use --governance-check off to skip' % exc, file=sys.stderr)
            return 5
        if findings and args.governance_check == 'strict':
            print('startup refused: the registry leaves the project governance: %s' % json.dumps(findings), file=sys.stderr)
            return 5
    journal = Journal(args.journal, now_ms)
    try:
        journal.open()
    except JournalError as exc:
        print('startup refused: %s' % exc, file=sys.stderr)
        return 2
    trace = None
    try:
        trace = TraceWriter(args.trace, event_keys, revoked)
        if trace.skipped:
            shown = ', '.join(str(n) for n in trace.skipped[:10]) + (', ...' if len(trace.skipped) > 10 else '')
            print('skipped %d trace line(s) that are not bridge records (lines %s); an event such a line held may be published again'
                  % (len(trace.skipped), shown), file=sys.stderr)
        problem = journal.check_anchor(trace.last_anchor)
        if problem:
            print('startup refused: %s' % problem, file=sys.stderr)
            return 6
        client = Ia2Client(host, int(port))
        probe = {'reachable': None, 'problems': []}
        if args.preflight == 'on':
            probe = preflight.check(client, reg)
            if probe['reachable'] and probe['problems']:
                print('startup refused: %s' % '; '.join(probe['problems']), file=sys.stderr)
                return 4
        hooks, hooks_sha, hooks_path = None, None, None
        # --- lab-only begin
        if args.test_hooks:
            from .testhooks_loader import load_hooks
            hooks, hooks_sha = load_hooks(args.test_hooks)
            hooks_path = args.test_hooks
        # --- lab-only end
        journal.append('start', build=build_flavor.FLAVOR, pid=os.getpid(), registry_sha256=reg.sha256, northbound=nb,
                       hooks=hooks_path, hooks_sha256=hooks_sha, torn_copy=journal.torn_copy, runtime=args.runtime,
                       poll_ms=args.poll_ms, governance_findings=findings, preflight=probe)
        cfg = {'poll_ms': args.poll_ms, 'advance_wait_ms': args.advance_wait_ms, 'hold_path': args.hold_file,
               'project_dir': args.project_dir, 'reconcile_recheck_ms': args.reconcile_recheck_ms,
               'enable_path': args.enable_file or args.journal + '.enable'}
        status_path = args.status_file or args.journal + '.status.json'
        engine = Engine(cfg, reg, keys, journal, client, trace, hooks)
        engine.apply_enables()
        engine.recover()
        try:
            write_status(status_path, engine.status_snapshot())      # a status file that cannot be written is refused here, not found out later
        except OSError as exc:
            print('startup refused: cannot write the status file %s (%s)' % (status_path, exc), file=sys.stderr)
            return 2
        print(json.dumps({'ready': True, 'pid': os.getpid(), 'registry_sha256': reg.sha256, 'build': build_flavor.FLAVOR}), flush=True)

        tail = RequestTail(args.requests, args.max_line_bytes)
        last_slow, reported = 0.0, set()
        while True:
            batch = tail.poll()
            arrived = now_ms()                    # one arrival time for the whole batch: time spent queued behind a slow line is not staleness
            for item in batch:
                if item[0] == 'line':
                    engine.safe_handle_line(item[1], arrived)
                else:
                    engine.note_oversize(item[1], item[2])
            if time.time() - last_slow >= 2.0:
                last_slow = time.time()
                for name, step in (('terminal results', engine.retry_terminal_events),
                                   ('operator enables', engine.apply_enables), ('reconcile', engine.reconcile_unknowns),
                                   ('notice', engine.maybe_notice), ('status file', lambda: write_status(status_path, engine.status_snapshot()))):
                    try:
                        step()
                    except JournalWriteError:
                        raise
                    except Exception as exc:  # noqa: BLE001 - housekeeping must not end the service; counted, and said once per step
                        engine.counters['internal_errors'] += 1
                        if name not in reported:
                            reported.add(name)
                            print('housekeeping step %r failed (%s: %s); the bridge keeps running' % (name, type(exc).__name__, exc), file=sys.stderr)
            time.sleep(0.05)
    except JournalWriteError as exc:
        print('journal failure: %s -- the bridge stops so that nothing more is sent; the next start replays what is on disk' % exc, file=sys.stderr)
        return 7
    finally:
        try:
            if trace is not None:
                trace.close()
        finally:
            journal.close()


if __name__ == '__main__':
    sys.exit(main())
