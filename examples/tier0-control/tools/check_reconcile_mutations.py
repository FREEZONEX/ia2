"""Require the reconciliation vectors (tests/test_reconcile_report.py) to detect deliberately broken variants of
tools/reconcile_report.py by an assertion, not by an import error or a crash.  `old-next-request-replaces` puts back the reviewed
rule that any later request for the variable replaced a value (REPAIR-ADVICE FIX01); the `*-link*`, `*-gap*` and `*-coverage*`
variants undo FIX02."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'evidence' / 'reconcile-mutations'
OUT.mkdir(parents=True, exist_ok=True)
NO_LINK = "        return self.sequenced or any(d0 <= t0 and t1 <= d1 for d0, d1 in self.down)"
CASES = [
    # FIX01: what may replace a value the device never got
    ('old-next-request-replaces', "                succ = replacement(var, mine[0], applied)\n",
     "                succ = {'upper': next_same, 'action_id': 'next request'} if next_same < a['t'] + 3600 else None\n"),
    ('unapplied-write-replaces', "        if (applied is None or e.get('name')", "        if (e.get('name')"),
    ('mismatched-entry-replaces', " or e.get('requested') != b['bits']\n", "\n"),
    ('http-conflict-replaces', "\n                or b['http_status'] not in (None, 200)):", "):"),
    ('same-value-replaces', "if c['idx'] > own['idx'] and c['applied'] != applied)", "if c['idx'] > own['idx'])"),
    ('ring-order-ignored', "if c['idx'] > own['idx'] and c['applied'] != applied)", "if c is not own and c['applied'] != applied)"),
    ('other-instance-replaces', "applied_in_ring.get((own['runtime_id'], var), [])",
     "[c for k, v in sorted(applied_in_ring.items()) if k[1] == var for c in v]"),
    ('requested-not-applied-explains-device', "set_events.append({'variable': var, 'bits': applied,",
     "set_events.append({'variable': var, 'bits': bits,"),
    ('restart-any-time-replaces', "end <= a['t'] + DEVICE_SLACK_S:", "(replaced_by == 'restart' or end <= a['t'] + DEVICE_SLACK_S):"),
    # FIX02: device log coverage before any inference from silence
    ('silence-is-no-link', NO_LINK, "        return True"),
    ('link-never-down', NO_LINK, "        return False"),
    ('no-link-without-coverage', "        if not self.covers(t0, t1) or self.answered(t0, t1):", "        if self.answered(t0, t1):"),
    ('link-down-until-an-incomplete-log-ends', "elif end is None and log.complete and log.no_link(", "elif end is None and log.no_link("),
    ('discrepancy-without-coverage', "elif log.covers(a['t'], until) and log.answered(a['t'], until):",
     "elif log.answered(a['t'], until):"),
    ('device-gap-counts-as-consistent', "                    gap = row['device'] = 'uncheckable_device_log'",
     "                    row['device'] = 'uncheckable_device_log'"),
    ('no-effect-without-coverage', "            elif not log.covers(a['t'], horizon + DEVICE_SLACK_S):", "            elif False:"),
    ('seq-gap-ignored', " or (self.sequenced and seqs[n] != seqs[n - 1] + 1):", ":"),
    ('stop-gap-ignored', "            if prev.get('ev') == 'stopped' or (", "            if ("),
    ('start-record-not-required', "if not self.records or self.records[0].get('ev') != 'listen':", "if not self.records:"),
    ('mixed-seq-accepted', "        if any((s is None) == self.sequenced for s in seqs):", "        if False:"),
    ('damaged-record-skipped', "read_jsonl(path, strict=True)", "read_jsonl(path)"),
    ('missing-role-accepted', "    if missing:\n", "    if False:\n"),
    ('evidence-gap-is-clean', "    incomplete = any(counts.get(c) for c in EVIDENCE_GAPS)", "    incomplete = False"),
    ('archive-silence-is-a-restart', "'uncheckable_restart' if tl.next_start_after(rid) is not None else 'uncheckable_archive'",
     "'uncheckable_restart'"),
    ('empty-archive-accepted', "    if not ainfo['polls_used']:", "    if False:"),
    # earlier rules (2026-10-29 first round)
    ('gap-never-detected', "    return cur, True                       # nothing in common", "    return cur, False                      # nothing in common"),
    ('full-first-read-trusted', "        return cur, len(cur) >= RING_CAP\n    if not prev:", "        return cur, False\n    if not prev:"),
    ('r3t1-ignores-the-http-answer', "                if a['http_status'] is None:\n                    row['class'] = 'executed_unaudited'",
     "                if True:\n                    row['class'] = 'executed_unaudited'"),
    ('unknown-writes-dropped', "for e in entries if e['entry'].get('origin') not in labels]", "for e in entries if False]"),
    ('stale-value-allowed-after-delivery', "if mine and (mine[-1]['delivered_t'] is None or d['t'] <= mine[-1]['delivered_t']):",
     "if mine:"),
    ('restart-never-ends-coverage', "        if inst is None or inst['last_poll'] < until:", "        if inst is None:"),
    ('broken-chain-still-clean', " or not jinfo['chain_ok']", ""),
    ('resend-not-checked', "    if wire and row['posts'] > 1:", "    if False:"),
    ('missing-delivery-not-checked', "                    problems.append('IA2 recorded %r as applied and the device was answering",
     "                    (lambda *_: None)('IA2 recorded %r as applied and the device was answering"),
    ('200-without-entry-accepted', "            elif a['http_status'] is not None:\n                problems.append('IA2 answered HTTP %s but /audit has no entry'",
     "            elif False:\n                problems.append('IA2 answered HTTP %s but /audit has no entry'"),
    ('safe-state-not-used', "            elif r.get('ev') == 'safe_state':", "            elif False:"),
    ('program-value-not-used', "        if 'program_value' in m and d['bits'] == m['program_value']:", "        if False:"),
    ('same-poll-trusted-without-history', "        same = poll.get('same') is True and rid in last", "        same = poll.get('same') is True"),
    ('two-entries-per-label-accepted', "            if len(mine) > 1:", "            if False:"),
]
results = []
for name, old, new in CASES:
    with tempfile.TemporaryDirectory(prefix='ia2-reconcile-mutation-') as tmp:
        dest = Path(tmp)
        for rel in ('tools/reconcile_report.py', 'tests/test_reconcile_report.py'):
            (dest / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / rel, dest / rel)
        path = dest / 'tools/reconcile_report.py'
        text = path.read_text()
        assert text.count(old) == 1, '%s: the original text occurs %d times' % (name, text.count(old))
        path.write_text(text.replace(old, new))
        p = subprocess.run([sys.executable, '-I', '-B', str(dest / 'tests/test_reconcile_report.py')],
                           capture_output=True, text=True, timeout=120)
        log = p.stdout + p.stderr
        detected = p.returncode != 0 and 'AssertionError' in log and 'FAILED (failures=' in log and 'ERROR:' not in log
        (OUT / (name + '.log')).write_text(log)
        row = {'mutation': name, 'detected_by_assertion': detected, 'exit_code': p.returncode,
               'failed': [l.split(' ', 1)[1] for l in log.splitlines() if l.startswith('FAIL: ')]}
        results.append(row)
        print(json.dumps({k: row[k] for k in ('mutation', 'detected_by_assertion', 'exit_code')}), flush=True)
(OUT / 'summary.json').write_text(json.dumps(results, indent=2) + '\n')
print('%d/%d detected by an assertion' % (sum(r['detected_by_assertion'] for r in results), len(results)))
sys.exit(0 if all(r['detected_by_assertion'] for r in results) else 1)
