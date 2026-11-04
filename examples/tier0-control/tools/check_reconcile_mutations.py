"""Require the reconciliation vectors (tests/test_reconcile_report.py) to detect deliberately broken variants of
tools/reconcile_report.py by an assertion, not by an import error or a crash.  `old-next-request-replaces` puts back the reviewed
rule that any later request for the variable replaced a value (REPAIR-ADVICE FIX01); the `*-link*`, `*-gap*` and `*-coverage*`
variants undo FIX02; the `*order*`, `device-*`, `*instance*`, `handover-*` and `*stretch*` variants undo the fixes of the
re-review (R1-R3, 2026-10-29), and `groups-*`, `*budget*`, `failed-branch-*` and the later `handover-*` ones those of the
independent review r2 (F1-F3, same day)."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OUT = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / 'evidence' / 'reconcile-mutations'   # [output directory]
OUT.mkdir(parents=True, exist_ok=True)
NO_LINK = "        return self.sequenced or any(d0 <= t0 and t1 <= d1 for d0, d1 in self.down)"
RIVALS = "        other = [c for c in applied_writes.get((rid, var), []) if c['bits'] != applied]"
COVERS = "        if not (math.isfinite(t0) and math.isfinite(t1)) or t0 > t1:"
DELIVERY = "                     and (rid is None or tl.surely(rid, d['t']))), None)"
CASES = [
    # FIX01: what may replace a value the device never got
    ('old-next-request-replaces', "                succ, unordered = rivals(inst, var, applied, lo_key, hi_key)\n",
     "                succ, unordered = ({'hi': (next_same, 0), 'action_id': 'next request'} if next_same < a['t'] + 3600 else None), []\n"),
    ('unapplied-write-replaces', "        if (applied is None or e.get('name')", "        if (e.get('name')"),
    ('mismatched-entry-replaces', " or e.get('requested') != b['bits']\n", "\n"),
    ('http-conflict-replaces', "\n                or b['http_status'] not in (None, 200)):", "):"),
    ('same-value-replaces', RIVALS, RIVALS.replace("if c['bits'] != applied]", "if c['lo'] != lo]")),
    ('other-instance-replaces', RIVALS, RIVALS.replace("applied_writes.get((rid, var), [])",
                                                       "[c for k, v in sorted(applied_writes.items(), key=str) if k[1] == var for c in v]")),
    ('requested-not-applied-explains-device', "set_events.append({'variable': var, 'bits': applied,",
     "set_events.append({'variable': var, 'bits': bits,"),
    ('restart-any-time-replaces', "elif end is not None and end <= lo + DEVICE_SLACK_S:",
     "elif end is not None and (replaced_by == 'restart' or end <= lo + DEVICE_SLACK_S):"),
    # re-review R2: the apply order comes from evidence, never from the ring, and the device check uses the same order
    ('overlap-counts-as-later', "        later = [c for c in other if c['lo'] > hi]", "        later = [c for c in other if c['hi'] > hi]"),
    ('unknown-order-is-a-discrepancy', "                elif unordered:\n", "                elif False:\n"),
    ('unknown-order-counts-as-consistent', "                    gap = row['device'] = 'uncheckable_order'\n                    row['unordered_with']",
     "                    row['device'] = 'uncheckable_order'\n                    row['unordered_with']"),
    ('order-gap-is-clean', "EVIDENCE_GAPS = ('uncheckable_archive', 'uncheckable_device_log', 'uncheckable_order')",
     "EVIDENCE_GAPS = ('uncheckable_archive', 'uncheckable_device_log')"),
    ('journal-order-ignored', "else (seen['first_seen'], math.inf)\n        return lo, max(lo, hi)",
     "else (seen['first_seen'], math.inf)\n        hi = (hi[0], math.inf)\n        return lo, max(lo, hi)"),
    ('device-check-in-intent-order', "    orders = [[p for p in itertools.permutations(range(len(g)))",
     "    orders = [[p for p in [tuple(range(len(g)))]"),
    ('device-check-accepts-any-order', "        if n not in unexplained and failed:", "        if False:"),
    # independent review r2: every full combination of group orders, and one shared handover
    ('groups-checked-one-at-a-time', "                key = (tuple((k, p) for k, p in fixed if k >= low), frozenset(nxt))",
     "                key = ((), frozenset(nxt))"),
    ('order-budget-overrun-is-clean', "                if m not in unexplained:\n                    depends.setdefault(m, set())",
     "                if False:\n                    depends.setdefault(m, set())"),
    ('failed-branch-names-only-its-last-group', "                failed |= history", "                failed |= {k for k, _ in fixed}"),
    ('handover-drops-order-gaps', "                elif {id(w) for w, _ in unchecked} - was_unchecked - was_bad and outcome == 'explained':",
     "                elif False:"),
    ('handover-may-go-back', "itertools.combinations_with_replacement(range(len(owners)), len(writes))",
     "itertools.product(range(len(owners)), repeat=len(writes))"),
    ('handover-blames-nobody', "            culprits = ids                       # each could be someone's",
     "            culprits = set()                     # each could be someone's"),
    ('handover-blames-every-write', "        culprits = set.intersection(*blamed) & ids if",
     "        culprits = ids if"),
    ('handover-budget-overrun-is-clean', "            unchecked_dev += [dict(d, runtime_id=None, possible_instances=list(owners)) for d in writes]\n            continue",
     "            continue"),
    ('unchecked-device-writes-are-clean', "    incomplete = any(counts.get(c) for c in EVIDENCE_GAPS) or unchecked_dev",
     "    incomplete = any(counts.get(c) for c in EVIDENCE_GAPS)"),
    ('device-proof-of-order-ignored', "            interval[b['action_id']] = (lo, (first['t'], math.inf))", "            pass"),
    ('device-proof-from-a-shared-value', "        if first and not any(c is not b and c['variable'] == var",
     "        if first and not any(False and c['variable'] == var"),
    ('earlier-values-need-not-be-applicable', "or any(g[order[r]]['lo'][0] > c for r in range(q + 1)):",
     "or g[order[q]]['lo'][0] > c:"),
    ('stale-value-allowed-after-delivery', "            for k in range(max(k0, last_sure - 1), last_possible + 1):",
     "            for k in range(max(0, last_sure - 1), last_possible + 1):"),
    # re-review R3: a delivery counts only for the instance that surely made it
    ('delivery-from-any-instance', DELIVERY, "                     ), None)"),
    ('handover-taken-as-known', "        return max(self.last_seen.get(rid, st), st)", "        return nxt"),
    ('unknown-instance-taken-as-the-old-one', "        if sure or len(owners) == 1:", "        if True:"),
    ('handover-delivery-counts-as-missing', "                elif device_maybe(var, applied, lo, look, inst) or sure_until",
     "                elif sure_until"),
    # re-review R1: coverage is an ordered, finite stretch inside the log
    ('reversed-stretch-covered', COVERS, "        if not (math.isfinite(t0) and math.isfinite(t1)):"),
    ('non-finite-time-accepted', "if any(not isinstance(t, (int, float)) or not math.isfinite(t) for t in times):",
     "if any(not isinstance(t, (int, float)) for t in times):"),
    ('a-moment-is-a-stretch', "        if not t0 < t1 or not self.covers(t0, t1)", "        if not self.covers(t0, t1)"),
    # FIX02: device log coverage before any inference from silence
    ('silence-is-no-link', NO_LINK, "        return True"),
    ('link-never-down', NO_LINK, "        return False"),
    ('no-link-without-coverage', "        if not t0 < t1 or not self.covers(t0, t1) or self.answered(t0, t1):",
     "        if not t0 < t1 or self.answered(t0, t1):"),
    ('link-down-until-an-incomplete-log-ends', "elif end is None and log.complete and log.no_link(", "elif end is None and log.no_link("),
    ('discrepancy-without-coverage', "elif log.covers(lo, sure_until) and log.answered(lo, sure_until):",
     "elif log.answered(lo, sure_until):"),
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
