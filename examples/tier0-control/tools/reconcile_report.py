#!/usr/bin/env python3
"""Three-way reconciliation (DESIGN-1A WP6, CONTRACT 8.3): the bridge journal <-> an archive of IA2 `/audit` <-> the devices' own
write logs.  Stdlib only, Python >= 3.8.  It reads files and never talks to the bridge, the runtime or a device, and it imports
nothing from `bridge/`: an auditor that shared the bridge's code would vouch for the bridge's mistakes.

  python3 -I tools/reconcile_report.py --journal journal.jsonl --audit-archive audit-archive.jsonl \\
      --device-map device-map.json --device-log 1=slave-1.jsonl --device-log 2=slave-2.jsonl \\
      [--lifecycle lifecycle.jsonl] [--wire wire.jsonl] [--out report.json]

Inputs
  journal        the bridge's hash-chained journal.  Every `intent` is a write the bridge may have sent.
  audit-archive  one JSON line per poll of the runtime: {"t": <unix s>, "runtime_id": ..., "runtime_id_after": ..., "audit": [...]};
                 a poll that read exactly what the previous poll of the same instance read may say "same": true instead of
                 repeating the list.  `/audit` is a 256-entry ring that a restart clears, so only an archive taken often enough
                 says what IA2 recorded.  A poll whose two runtime_id reads differ, or that has neither, is ignored.  Any input
                 file may be gzip-compressed (`.gz`).
  device-map     {"<IA2 variable>": {"device": "<role>", "kind": "f32"|"i32"|"coil", "addr": <int>}} for the written outputs;
                 "program_value": <bits> marks an output the PLC program itself assigns on every scan (that value is its own), and
                 "failsafe": <bits> the value a safe state writes (default 0: the Modbus adapter zeroes outputs).  Every device
                 role it names needs a --device-log.
  device-log     <role>=<file>: the lab's fake device log; only requests it answered (`action: resp`) reached the device.  It
                 must parse completely and start with the device's `listen` record; it covers the time from there to its last
                 record, which for a complete log is `stopped`.  Its `mode` records are explicit link states (silent, close and
                 refuse are no working link).  A `stopped` followed by more records, or a `seq` that does not go up by one, is a
                 gap in what the log covers.  Logs written before `seq` was added (2026-10-29) are accepted without it: they
                 cannot show a missing record, so silence in them proves nothing on its own.
  lifecycle      optional: {"t", "ev": "runtime_ready", "runtime_id", "spawned_t"} lines; without them a runtime instance starts at
                 the first sighting of its runtime_id (journal or archive), which is after it wrote its initial outputs.
                 {"t", "ev": "safe_state", "why"} marks the moment the instance was told to stop or made to latch: from then on
                 until the next instance its outputs may be driven to their failsafe value.
  wire           optional: the recording proxy's log, to tell "never sent" from "sent, no effect".

Classes (one per journal intent)
  consistent            /audit has exactly one entry for the label, it matches the intent and the bridge's own answer, and the
                        device shows the applied value -- or there is a reason it does not: IA2 said it was not delivered, the
                        device already held it, the device got the request and did not answer, a newer value or a restart
                        replaced it within one output cycle, or the device log shows no working link from the write until a newer
                        value or a restart replaced it (or until a complete log ends).  Only a write whose /audit entry records
                        another applied value, in the same runtime instance and later in its ring, replaces a value; a denied,
                        unsent or unanswered request does not.
  executed_unaudited    no entry although the archive covers the write, the device received the value, and the bridge got no HTTP
                        answer: the runtime applied a write whose client had gone and did not record it (CONTRACT 11.5).
  not_sent              no entry, no device effect, and the proxy saw no POST with the label (the bridge stopped between its intent
                        and the send; R1).
  no_effect_no_entry    no entry, no device effect, no HTTP answer: consistent with a request the runtime never read (R3).
  uncheckable_restart   the runtime instance ended (a later instance was seen) before the archive could see an entry made for
                        this write.
  uncheckable_turnover  the ring turned over between two polls while this write could have been recorded.
  uncheckable_archive   the archive stopped seeing the instance before it could have seen an entry, and nothing shows the instance
                        ended: an evidence gap.
  uncheckable_device_log  the answer depends on the device log, and the log does not cover the time it would have to (it ends
                        early or has a gap), or it is silent there without proof that it was recording: an evidence gap.
  unexplained           anything else: an entry that disagrees, two entries, an HTTP answer without an entry, a device write
                        without an entry while the bridge had an answer, an applied value the device never got although it was
                        answering the runtime.

Plus, outside any intent:
  unknown_writes            audit entries whose origin is no label in the journal (CONTRACT 8.3 "unknown write": alert, keep).
  unexplained_device_writes device writes whose value is neither the instance's initial value, nor a value one of the bridge's
                            recent writes to that variable put into the VM, nor the program's own value for an output it
                            assigns, nor the failsafe value after a safe_state.

The verdict is DISCREPANCY when there is an unknown write, an unexplained intent or device write, or the journal chain is broken;
otherwise INCOMPLETE when an intent is uncheckable_archive or uncheckable_device_log; otherwise CLEAN.  Exit 0 if CLEAN, 1 on a
discrepancy, 2 if INCOMPLETE (the report is still written) or the input is unusable (no report).  Limits: an archive entry has no
ID, so two byte-identical entries in a row are counted once (the wire log's POST count per label is the check for a resend);
IA2's audit time has second resolution, so the order of two writes comes from the ring and their times from the bridge's clock,
which in the lab is the device log's clock too."""
import argparse
import bisect
import gzip
import hashlib
import json
import os
import sys

RING_CAP = 256
COVER_S = 10.0          # a write may execute this long after its intent (queued behind a stopped runtime, a retried device); the
                        # archive must see the same runtime instance, without a gap, until then for "no entry" to mean anything
DEVICE_SLACK_S = 2.0    # the runtime writes outputs on change, once per I/O cycle (100 ms in the lab)
EVIDENCE_GAPS = ('uncheckable_archive', 'uncheckable_device_log')
GENESIS = '0' * 64


class InputError(Exception):
    pass


def read_jsonl(path, strict=False):
    out = []
    with (gzip.open if path.endswith('.gz') else open)(path, 'rb') as f:
        for n, raw in enumerate(f.read().split(b'\n'), 1):
            if not raw.strip():
                continue
            try:
                out.append(json.loads(raw.decode('utf-8')))
            except ValueError:
                if strict:
                    raise InputError('%s line %d is not JSON' % (path, n))
    return out


def sha256_file(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


# ---- journal ----------------------------------------------------------------------------------------------------------------
def load_journal(path):
    """Intents with what the bridge learned about each; the hash chain is checked from the bytes alone."""
    prev, seq, chain_ok = GENESIS, 0, True
    actions, order = {}, []
    with open(path, 'rb') as f:
        lines = f.read().split(b'\n')
    for raw in lines[:-1]:
        try:
            rec = json.loads(raw.decode('ascii'))
        except ValueError:
            chain_ok = False
            break
        if rec.get('prev') != prev or rec.get('seq') != seq + 1:
            chain_ok = False
        prev, seq = hashlib.sha256(raw).hexdigest(), seq + 1
        aid, kind = rec.get('action_id'), rec.get('kind')
        if kind == 'intent':
            if aid in actions:
                raise InputError('journal has two intents for %s' % aid)
            actions[aid] = {'action_id': aid, 'label': rec.get('label'), 'variable': rec.get('variable'), 'bits': rec.get('bits'),
                            'target_key': rec.get('target_key'), 't': rec['t_ms'] / 1000.0, 'runtime_id': rec.get('runtime_id'),
                            'http_status': None, 'transfer': None, 'response_t': None, 'terminal': None}
            order.append(aid)
        elif aid in actions and kind == 'response':
            http = rec.get('http') or {}
            a = actions[aid]
            a['transfer'] = http.get('kind')                                  # 'response' | 'error'
            a['http_status'] = http.get('status') if http.get('kind') == 'response' else None
            a['response_t'] = rec['t_ms'] / 1000.0
        elif aid in actions and kind == 'terminal':
            ev = rec.get('event') or {}
            actions[aid]['terminal'] = [ev.get('status'), ev.get('reason')]
    return [actions[a] for a in order], {'records': seq, 'chain_ok': chain_ok, 'torn_tail': bool(lines[-1])}


# ---- audit archive ----------------------------------------------------------------------------------------------------------
def entry_key(e):
    return json.dumps(e, sort_keys=True, separators=(',', ':'))


def new_entries(prev, cur):
    """Entries `cur` added after `prev` (two reads of one ring, oldest first; `prev` None for an instance's first read) and whether
    some may have been lost in between.  The ring holds the newest RING_CAP, so cur == (prev + new)[-RING_CAP:]; the largest overlap
    is the fewest new entries.  A first read that is already full may have lost the instance's oldest entries."""
    if prev is None:
        return cur, len(cur) >= RING_CAP
    if not prev:
        return cur, len(cur) >= RING_CAP
    if len(cur) < RING_CAP:
        if cur[:len(prev)] != prev:
            raise InputError('an audit read is not an extension of the previous read of the same runtime')
        return cur[len(prev):], False
    for k in range(min(len(prev), len(cur)), 0, -1):
        if prev[len(prev) - k:] == cur[:k]:
            return cur[k:], False
    return cur, True                       # nothing in common: RING_CAP or more entries came in between; any number may be lost


def load_archive(path):
    """Distinct audit entries per runtime instance with the time they were first seen, plus every turnover gap."""
    instances, entries, gaps, used, ignored = {}, [], [], 0, 0
    last = {}
    for poll in read_jsonl(path):
        rid, audit, t = poll.get('runtime_id'), poll.get('audit'), poll.get('t')
        same = poll.get('same') is True and rid in last
        if not rid or rid != poll.get('runtime_id_after') or not (same or isinstance(audit, list)) or not isinstance(t, (int, float)):
            ignored += 1
            continue
        used += 1
        keys = last[rid][1] if same else [entry_key(e) for e in audit]
        inst = instances.setdefault(rid, {'runtime_id': rid, 'first_poll': t, 'last_poll': t, 'polls': 0, 'entries': 0})
        prev_t, prev_keys = last.get(rid, (None, None))
        added, gap = new_entries(prev_keys, keys)
        if gap:
            gaps.append({'runtime_id': rid, 'after': prev_t, 'before': t})
        for k in added:            # in ring order: `idx` orders the entries of one instance
            e = json.loads(k)
            entries.append({'runtime_id': rid, 'first_seen': t, 'entry': e, 'idx': len(entries)})
        inst['entries'] += len(added)
        inst['polls'] += 1
        inst['last_poll'] = t
        last[rid] = (t, keys)
    return entries, gaps, instances, {'polls_used': used, 'polls_ignored': ignored}


# ---- devices ----------------------------------------------------------------------------------------------------------------
def signed32(u):
    u &= 0xFFFFFFFF
    return u - (1 << 32) if u & 0x80000000 else u


DOWN_MODES = ('silent', 'close', 'refuse')


class DeviceLog:
    """What one fake device's log can vouch for: the stretch it covers (from its `listen` to its last record, minus gaps), its
    explicit link-down intervals, and the time of every request it answered.  The runtime polls each device every 100 ms in the
    lab, so a covered stretch with no answered request had no working link -- provided the log shows it was recording: an explicit
    link-down `mode`, or `seq` numbers that prove no record is missing (the device and its log are one process)."""

    def __init__(self, role, path):
        self.role, self.records = str(role), read_jsonl(path, strict=True)
        if not self.records or self.records[0].get('ev') != 'listen':
            raise InputError('device log %s=%s does not start with the device\'s `listen` record' % (role, path))
        seqs = [r.get('seq') for r in self.records]
        self.sequenced = seqs[0] is not None
        if any((s is None) == self.sequenced for s in seqs):
            raise InputError('device log %s mixes records with and without `seq`' % role)
        times = [r.get('t', r.get('t_req')) for r in self.records]
        if any(not isinstance(t, (int, float)) for t in times):
            raise InputError('device log %s has a record without a time' % role)
        self.start, self.end, self.complete = times[0], times[-1], self.records[-1].get('ev') == 'stopped'
        self.gaps, self.down, self.served = [], [], []
        mode, since = 'normal', self.start
        for n in range(1, len(self.records)):
            r, prev, t = self.records[n], self.records[n - 1], times[n]
            if prev.get('ev') == 'stopped' or (self.sequenced and seqs[n] != seqs[n - 1] + 1):
                self.gaps.append([times[n - 1], t])
            if r.get('ev') == 'mode':
                if mode in DOWN_MODES:
                    self.down.append([since, t])
                mode, since = r.get('mode'), t
            if r.get('ev') == 'req' and r.get('action') == 'resp':
                self.served.append(r.get('t_req', t))
        if mode in DOWN_MODES:
            self.down.append([since, self.end])
        self.served.sort()

    def covers(self, t0, t1):
        return self.start <= t0 and t1 <= self.end and not any(g0 < t1 and g1 > t0 for g0, g1 in self.gaps)

    def answered(self, t0, t1):
        return bisect.bisect_right(self.served, t1) > bisect.bisect_left(self.served, t0)

    def no_link(self, t0, t1):
        """Covered from t0 to t1, no request answered in between, and proof the log was recording: an explicit link-down
        interval that spans it, or an unbroken `seq`."""
        if not self.covers(t0, t1) or self.answered(t0, t1):
            return False
        return self.sequenced or any(d0 <= t0 and t1 <= d1 for d0, d1 in self.down)

    def summary(self):
        return {'start': self.start, 'end': self.end, 'complete': self.complete, 'sequenced': self.sequenced, 'gaps': self.gaps,
                'link_down_intervals': len(self.down), 'answered_requests': len(self.served)}


def load_device_writes(device_map, logs):
    """(variable, bits as IA2's i32, t) for every write a device answered at a mapped output address, and separately every write
    that reached a device which did not answer it (silent / closed link: the runtime tried, the device never took it).  Also the
    DeviceLog of each role."""
    by_addr = {}
    for var, m in device_map.items():
        by_addr[(str(m['device']), 'coil' if m['kind'] == 'coil' else 'reg', int(m['addr']))] = (var, m['kind'])
    missing = sorted({str(m['device']) for m in device_map.values()} - {str(r) for r in logs})
    if missing:
        raise InputError('no --device-log for device role(s) %s named in the device map' % ', '.join(missing))
    out, attempts, devlogs = [], [], {}
    for role, path in logs.items():
        devlogs[str(role)] = log = DeviceLog(role, path)
        for r in log.records:
            if r.get('ev') != 'req' or r.get('fc') not in (5, 15, 6, 16):
                continue
            fc, t = r['fc'], r.get('t_req', r.get('t'))
            space = 'coil' if fc in (5, 15) else 'reg'
            hit = by_addr.get((str(role), space, r.get('addr')))
            if hit is None:
                continue
            var, kind = hit
            if kind == 'coil':
                bits = (1 if r.get('value') == 0xFF00 else 0) if fc == 5 else (r.get('bits') or [0])[0]
            elif fc == 16 and len(r.get('words') or []) >= 2:
                w = r['words']
                bits = signed32((w[0] << 16) | w[1])
            else:
                continue                   # a 32-bit output written with one register: not something this runtime does
            (out if r.get('action') == 'resp' else attempts).append({'variable': var, 'bits': bits, 't': t, 'device': str(role)})
    out.sort(key=lambda d: d['t'])
    attempts.sort(key=lambda d: d['t'])
    return out, attempts, devlogs


# ---- the reconciliation -----------------------------------------------------------------------------------------------------
class Timeline:
    """Which runtime instance was live when.  From the lifecycle log when there is one, else from first sightings."""

    def __init__(self, starts):
        self.starts = sorted(starts)      # (t, runtime_id)

    def at(self, t):
        cur = None
        for st, rid in self.starts:
            if st <= t:
                cur = rid
        return cur

    def start_of(self, rid):
        return next((st for st, r in self.starts if r == rid), None)

    def next_start_after(self, rid):
        st = self.start_of(rid)
        later = [s for s, r in self.starts if st is not None and s > st]
        return min(later) if later else None


def reconcile(journal, archive, device_map, device_logs, lifecycle=None, wire=None):
    actions, jinfo = load_journal(journal)
    entries, gaps, instances, ainfo = load_archive(archive)
    if not ainfo['polls_used']:
        raise InputError('the audit archive has no usable poll')
    with open(device_map) as f:
        dmap = json.load(f)
    dev, attempts, devlogs = load_device_writes(dmap, device_logs)
    starts, safe_states = {}, []
    if lifecycle:          # an instance writes its initial outputs before it reports ready: it starts when it was spawned
        for r in read_jsonl(lifecycle):
            if r.get('ev') == 'runtime_ready' and r.get('runtime_id'):
                starts.setdefault(r['runtime_id'], r.get('spawned_t', r['t']))
            elif r.get('ev') == 'safe_state':
                safe_states.append(r['t'])
    for a in actions:
        if a['runtime_id']:
            starts.setdefault(a['runtime_id'], a['t'])
    for rid, inst in instances.items():
        starts.setdefault(rid, inst['first_poll'])
    tl = Timeline([(t, rid) for rid, t in starts.items()])
    posts = {}
    if wire:
        for r in read_jsonl(wire):
            if r.get('ev') == 'req' and r.get('method') == 'POST':
                posts[r.get('origin')] = posts.get(r.get('origin'), 0) + 1

    by_label = {}
    for e in entries:
        by_label.setdefault(e['entry'].get('origin'), []).append(e)
    labels = {a['label'] for a in actions}

    def device_after(var, bits, t0, t1, log=None):
        return next((d for d in (dev if log is None else log) if d['variable'] == var and d['bits'] == bits and t0 - 0.05 <= d['t'] <= t1), None)

    def device_write_before(var, t):
        prior = [d for d in dev if d['variable'] == var and d['t'] < t]
        return prior[-1] if prior else None

    def covered(a, until):
        """(covered, why not): the archive saw a's runtime instance, gap-free, from the intent until `until`."""
        rid = a['runtime_id']
        inst = instances.get(rid)
        if inst is None or inst['last_poll'] < until:
            # A later instance is evidence this one ended; without one the archive merely stopped looking.
            return False, 'uncheckable_restart' if tl.next_start_after(rid) is not None else 'uncheckable_archive'
        if any(g['runtime_id'] == rid and g['before'] >= a['t'] and (g['after'] is None or g['after'] <= until) for g in gaps):
            return False, 'uncheckable_turnover'
        return True, None

    # Writes /audit shows applied: the label's one entry matches its intent, records an applied value and agrees with the bridge's
    # HTTP answer, if any (an applied write whose client left has no answer, and still counts).  Per (instance, variable), in ring
    # order.  `upper` is a time by which it had been applied: the bridge's answer, else the poll that first saw the entry.
    applied_in_ring = {}
    for b in actions:
        theirs = by_label.get(b['label'], [])
        e = theirs[0]['entry'] if len(theirs) == 1 else {}
        applied = e.get('applied')
        if (applied is None or e.get('name') != b['variable'] or e.get('requested') != b['bits']
                or b['http_status'] not in (None, 200)):
            continue
        upper = b['response_t'] if b['transfer'] == 'response' else theirs[0]['first_seen']
        applied_in_ring.setdefault((theirs[0]['runtime_id'], b['variable']), []).append(
            {'action_id': b['action_id'], 'idx': theirs[0]['idx'], 'applied': applied, 'upper': upper})
    for later in applied_in_ring.values():
        later.sort(key=lambda c: c['idx'])

    def replacement(var, own, applied):
        """The first write after `own` in its instance's ring that applied another value to `var`.  The same value again does
        not end the duty to deliver it."""
        return next((c for c in applied_in_ring.get((own['runtime_id'], var), []) if c['idx'] > own['idx'] and c['applied'] != applied),
                    None)

    rows, set_events = [], []
    for a in actions:
        var, bits = a['variable'], a['bits']
        mine = by_label.get(a['label'], [])
        next_same = min([b['t'] for b in actions if b['variable'] == var and b['t'] > a['t']] or [a['t'] + 3600])
        horizon = min(next_same, a['t'] + COVER_S)   # attributing a device write to a request with no entry: before the next request
        log = devlogs[str(dmap[var]['device'])]
        row = {'action_id': a['action_id'], 'label': a['label'], 'variable': var, 'bits': bits, 't': a['t'],
               'terminal': a['terminal'], 'http_status': a['http_status'], 'transfer': a['transfer'], 'audit_entries': len(mine)}
        if wire:
            row['posts'] = posts.get(a['label'], 0)
        problems = []
        if wire and row['posts'] > 1:
            problems.append('the proxy saw %d POSTs with this label (a resend)' % row['posts'])
        if mine:
            e = mine[0]['entry']
            if len(mine) > 1:
                problems.append('%d audit entries for one label' % len(mine))
            if e.get('name') != var or e.get('requested') != bits:
                problems.append('the entry does not match the intent: %r' % e)
            if mine[0]['runtime_id'] != a['runtime_id']:
                row['note'] = 'executed by the next runtime instance (it restarted between the gates and the send)'
            result, applied = str(e.get('result')), e.get('applied')
            st = a['http_status']
            if st == 200 and applied is None:
                problems.append('the bridge got 200 but the entry says nothing was applied: %r' % result)
            if st is not None and st != 200 and applied is not None:
                problems.append('the bridge got %s but the entry says %r was applied' % (st, applied))
            row['audit_result'] = result
            gap = None
            if applied is not None:
                set_events.append({'variable': var, 'bits': applied, 't': a['t'], 'runtime_id': mine[0]['runtime_id']})
                # The value must reach the device until a newer applied value or the end of its instance takes over.  One that
                # waits for a link (undelivered, or a device IA2 still believes healthy) arrives when the link returns.
                succ = replacement(var, mine[0], applied)
                ends = tl.next_start_after(mine[0]['runtime_id'])
                stops = ([(succ['upper'], succ['action_id'])] if succ else []) + ([(ends, 'restart')] if ends is not None and ends > a['t'] else [])
                end, replaced_by = min(stops) if stops else (None, None)
                until = end if end is not None else log.end
                seen = device_after(var, applied, a['t'], until + DEVICE_SLACK_S if end is not None else until)
                prior = device_write_before(var, a['t'])
                row['device'] = 'seen' if seen else 'not_seen'
                if seen:
                    pass
                elif prior and prior['bits'] == applied and log.covers(prior['t'], a['t']):
                    row['device'] = 'already_held'
                elif device_after(var, applied, a['t'], until + DEVICE_SLACK_S, attempts):
                    row['device'] = 'sent_unanswered'
                elif 'not delivered' in result:
                    row['device'] = 'undelivered'
                elif end is not None and end <= a['t'] + DEVICE_SLACK_S:
                    row['device'], row['replaced_by'] = 'replaced_before_delivery', replaced_by   # one output cycle: merged
                elif end is not None and log.no_link(a['t'], end):
                    # IA2 had not noticed yet that the link was gone (it answered 200, nothing about delivery); the device's own
                    # log shows no working link from the write until a newer value or a restart replaced it
                    row['device'], row['replaced_by'] = 'link_down_until_replaced', replaced_by
                elif end is None and log.complete and log.no_link(a['t'], log.end):
                    row['device'] = 'link_down_until_log_end'
                elif log.covers(a['t'], until) and log.answered(a['t'], until):
                    problems.append('IA2 recorded %r as applied and the device was answering the runtime, but it never received it'
                                    % applied)
                else:
                    gap = row['device'] = 'uncheckable_device_log'
            row['class'] = 'unexplained' if problems else gap or 'consistent'
        else:
            seen = device_after(var, bits, a['t'], horizon + DEVICE_SLACK_S)
            ok, why = covered(a, (seen['t'] if seen else a['t']) + COVER_S)
            row['device'] = 'seen' if seen else 'not_seen'
            prior = seen and device_write_before(var, seen['t'])
            if prior and prior['bits'] == bits:
                row['device'] = 'seen_unchanged'
            if wire and row['posts'] == 0 and not seen:
                row['class'] = 'not_sent'          # the proxy is the evidence; the archive's coverage does not matter
            elif not ok:
                row['class'] = why
                if seen:
                    set_events.append({'variable': var, 'bits': bits, 't': a['t'], 'runtime_id': a['runtime_id']})
            elif seen and row['device'] == 'seen':
                set_events.append({'variable': var, 'bits': bits, 't': a['t'], 'runtime_id': a['runtime_id']})
                if a['http_status'] is None:
                    row['class'] = 'executed_unaudited'
                else:
                    problems.append('IA2 answered HTTP %s and the device received the value, but /audit has no entry' % a['http_status'])
            elif a['http_status'] is not None:
                problems.append('IA2 answered HTTP %s but /audit has no entry' % a['http_status'])
            elif not log.covers(a['t'], horizon + DEVICE_SLACK_S):
                row['class'] = 'uncheckable_device_log'    # "no device effect" needs a log that was there to see one
            else:
                row['class'] = 'no_effect_no_entry'
            if problems:
                row['class'] = 'unexplained'
        if problems:
            row['problems'] = problems
        rows.append(row)

    unknown = [dict(e['entry'], runtime_id=e['runtime_id'], first_seen=e['first_seen'])
               for e in entries if e['entry'].get('origin') not in labels]

    # A device write is explained by the newest value the bridge had asked this instance to put into that variable, or -- until that
    # value has reached the device -- by the one before it (the instance's initial output, 0, before the first).  An older value
    # arriving after a newer one was delivered is a write nobody asked for.
    for s in set_events:
        hit = device_after(s['variable'], s['bits'], s['t'], s['t'] + 3600)
        s['delivered_t'] = hit['t'] if hit else None
    unexplained_dev = []
    for d in dev:
        rid, m = tl.at(d['t']), dmap[d['variable']]
        if 'program_value' in m and d['bits'] == m['program_value']:
            continue
        if d['bits'] == m.get('failsafe', 0) and any(ts - 0.05 <= d['t'] and tl.at(ts) == rid for ts in safe_states):
            continue
        mine = [s for s in set_events if s['variable'] == d['variable'] and s['runtime_id'] == rid and s['t'] <= d['t'] + 0.05]
        allowed = {mine[-1]['bits']} if mine else {0}
        if mine and (mine[-1]['delivered_t'] is None or d['t'] <= mine[-1]['delivered_t']):
            allowed.add(mine[-2]['bits'] if len(mine) > 1 else 0)
        if d['bits'] not in allowed:
            unexplained_dev.append(dict(d, runtime_id=rid, allowed=sorted(allowed)))

    counts = {}
    for r in rows:
        counts[r['class']] = counts.get(r['class'], 0) + 1
    discrepancy = unknown or unexplained_dev or counts.get('unexplained') or not jinfo['chain_ok']
    incomplete = any(counts.get(c) for c in EVIDENCE_GAPS)
    return {
        'verdict': 'DISCREPANCY' if discrepancy else 'INCOMPLETE' if incomplete else 'CLEAN',
        'counts': dict(sorted(counts.items()), intents=len(rows), unknown_writes=len(unknown),
                       unexplained_device_writes=len(unexplained_dev), device_writes=len(dev)),
        'journal': jinfo,
        'audit_archive': dict(ainfo, entries=len(entries), turnover_gaps=gaps,
                              instances=sorted(instances.values(), key=lambda i: i['first_poll'])),
        'device_logs': {role: log.summary() for role, log in sorted(devlogs.items())},
        'unknown_writes': unknown,
        'unexplained': [r for r in rows if r['class'] == 'unexplained'],
        'unexplained_device_writes': unexplained_dev,
        'actions': rows,
    }


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--journal', required=True)
    ap.add_argument('--audit-archive', required=True)
    ap.add_argument('--device-map', required=True)
    ap.add_argument('--device-log', action='append', default=[], metavar='ROLE=FILE')
    ap.add_argument('--lifecycle')
    ap.add_argument('--wire')
    ap.add_argument('--out')
    args = ap.parse_args(argv)
    logs = {}
    for spec in args.device_log:
        role, _, path = spec.partition('=')
        if not role or not path:
            ap.error('--device-log takes ROLE=FILE')
        logs[role] = path
    try:
        report = reconcile(args.journal, args.audit_archive, args.device_map, logs, args.lifecycle, args.wire)
    except (InputError, OSError, KeyError, ValueError) as exc:
        print('reconcile_report: unusable input: %s' % exc, file=sys.stderr)
        return 2
    srcs = [args.journal, args.audit_archive, args.device_map] + list(logs.values()) + [p for p in (args.lifecycle, args.wire) if p]
    report['inputs'] = {os.path.basename(p): sha256_file(p) for p in srcs}
    text = json.dumps(report, indent=1, sort_keys=True)
    if args.out:
        with open(args.out, 'w') as f:
            f.write(text + '\n')
    print(json.dumps({'verdict': report['verdict'], 'counts': report['counts']}, sort_keys=True))
    if report['verdict'] == 'INCOMPLETE':
        print('reconcile_report: evidence gap: %s' % ', '.join('%d %s' % (report['counts'][c], c) for c in EVIDENCE_GAPS
                                                                if report['counts'].get(c)), file=sys.stderr)
    return {'CLEAN': 0, 'DISCREPANCY': 1, 'INCOMPLETE': 2}[report['verdict']]


if __name__ == '__main__':
    sys.exit(main())
