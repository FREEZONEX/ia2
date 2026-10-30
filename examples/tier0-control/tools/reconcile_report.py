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
                 the first sighting of its runtime_id (journal or archive), which is after it wrote its initial outputs, and the
                 instance before it is known to have lived only until the archive last read it: a device write in between could
                 be either one's.  {"t", "ev": "safe_state", "why"} marks the moment the instance was told to stop or made to
                 latch: from then on until the next instance its outputs may be driven to their failsafe value.
  wire           optional: the recording proxy's log, to tell "never sent" from "sent, no effect".

Classes (one per journal intent)
  consistent            /audit has exactly one entry for the label, it matches the intent and the bridge's own answer, and the
                        device shows the applied value while the same runtime instance was live -- or there is a reason it does
                        not: IA2 said it was not delivered, the device already held it, the device got the request and did not
                        answer, a newer value or the end of the instance replaced it within one output cycle, or the device log
                        shows no working link from the write until a newer value or the end of the instance replaced it (or until
                        a complete log ends).  Only a write whose /audit entry records another applied value, in the same runtime
                        instance and known to have been applied after this one (see Order), replaces a value; a denied, unsent or
                        unanswered request does not.
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
  uncheckable_order     the value never reached the device while its instance was surely live, and the answer depends on an order
                        the evidence does not fix: another value applied in an unknown order with this one (it may have replaced
                        it at once, or come first), or the device got the value only while it is unknown which of two instances
                        was live (no lifecycle record of the handover): an evidence gap.
  unexplained           anything else: an entry that disagrees, two entries, an HTTP answer without an entry, a device write
                        without an entry while the bridge had an answer, an applied value the device never got although it was
                        answering the runtime.

Plus, outside any intent:
  unknown_writes            audit entries whose origin is no label in the journal (CONTRACT 8.3 "unknown write": alert, keep).
  unexplained_device_writes device writes of a variable that no full order of the values its instance applied explains (see
                            Order): the device follows the VM, so it may miss a value (outputs go out once per I/O cycle) and
                            may still get the one before the newest until the newest arrives, but it never gets a value
                            nothing put into the VM (the instance's initial value is 0), and never an older one after a newer
                            one arrived.  The program's own value for an output it assigns, and the failsafe value after a
                            safe_state, are always explained.  Device writes made while it is unknown which instance was live
                            (no lifecycle record of the handover) are checked together: the handover happened at one moment,
                            so each split of them in time order between the instances that may have made them is tried; the
                            ones no split explains are unexplained.
  unchecked_device_writes   device writes that some full order the evidence allows explains and another does not, with the
                            actions whose order decides it; handover writes that some splits explain and others do not; and
                            writes not searched because a group holds more than ORDER_CAP values, the search would follow more
                            than BRANCH_CAP combinations of group orders at once, or a handover has more than HANDOVER_CAP
                            splits: an evidence gap.

Order.  The runtime's scan thread applies a write and acknowledges it; the request task then answers HTTP and records the /audit
entry, in whatever order the tasks get the audit lock.  So the ring does not say which of two writes was applied first, and IA2's
audit time has second resolution.  What the evidence does fix: a write was applied after its intent (the bridge journals it
before sending) and before the bridge got the answer -- or, with none, before the archive first saw its entry -- and before a
device first got the value after the intent, if no other write of the bridge could have put that value into the variable by
then (outputs carry the VM's values).  One write was applied before another only if that interval ends before the other's
begins; two bridge records in one millisecond are compared by their place in the journal.  Writes whose intervals overlap may
have been applied either way round, and a check that depends on which came first is not decided by guessing.  All times are
the bridge's and the archive's clocks, which in the lab are the device log's clock too.

The verdict is DISCREPANCY when there is an unknown write, an unexplained intent or device write, or the journal chain is broken;
otherwise INCOMPLETE when an intent is uncheckable_archive, uncheckable_device_log or uncheckable_order, or a device write is
unchecked; otherwise CLEAN.  Exit 0 if CLEAN, 1 on a discrepancy, 2 if INCOMPLETE (the report is still written) or the input is
unusable (no report).  Limits: an archive entry has no ID, so two byte-identical entries in a row are counted once (the wire
log's POST count per label is the check for a resend)."""
import argparse
import bisect
import gzip
import hashlib
import itertools
import json
import math
import os
import sys

RING_CAP = 256
COVER_S = 10.0          # a write may execute this long after its intent (queued behind a stopped runtime, a retried device); the
                        # archive must see the same runtime instance, without a gap, until then for "no entry" to mean anything
DEVICE_SLACK_S = 2.0    # the runtime writes outputs on change, once per I/O cycle (100 ms in the lab)
ORDER_CAP = 6           # values in an unknown order with one another that the device check searches every order of (6! = 720)
BRANCH_CAP = 4096       # combinations of group orders the device check follows at once; past it the rest is unchecked
HANDOVER_CAP = 4096     # splits of the device writes made during one unrecorded handover; past it they are unchecked
EVIDENCE_GAPS = ('uncheckable_archive', 'uncheckable_device_log', 'uncheckable_order')
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
                            'http_status': None, 'transfer': None, 'response_t': None, 'terminal': None, 'intent_n': seq}
            order.append(aid)
        elif aid in actions and kind == 'response':
            http = rec.get('http') or {}
            a = actions[aid]
            a['transfer'] = http.get('kind')                                  # 'response' | 'error'
            a['http_status'] = http.get('status') if http.get('kind') == 'response' else None
            a['response_t'], a['response_n'] = rec['t_ms'] / 1000.0, seq
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
        if any(not isinstance(t, (int, float)) or not math.isfinite(t) for t in times):
            raise InputError('device log %s has a record without a finite time' % role)
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
        """The log vouches for every moment from t0 to t1: finite, in order, inside the log and clear of its gaps.  A log that
        ends before t0 vouches for nothing after its end."""
        if not (math.isfinite(t0) and math.isfinite(t1)) or t0 > t1:
            return False
        return self.start <= t0 and t1 <= self.end and not any(g0 < t1 and g1 > t0 for g0, g1 in self.gaps)

    def answered(self, t0, t1):
        return bisect.bisect_right(self.served, t1) > bisect.bisect_left(self.served, t0)

    def no_link(self, t0, t1):
        """Covered from t0 to t1 (a stretch of time, not a moment), no request answered in between, and proof the log was
        recording: an explicit link-down interval that spans it, or an unbroken `seq`."""
        if not t0 < t1 or not self.covers(t0, t1) or self.answered(t0, t1):
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
def apply_groups(events):
    """The values one instance applied to one variable ({'bits', 'lo', 'hi'}: applied somewhere between the (time, journal
    place) keys lo and hi) as groups in their known order: every value of a group was applied before every value of the next,
    and within a group the order is known only where one interval ends before another begins.  The first group is the
    instance's initial value, 0."""
    never = (-math.inf, -math.inf)
    groups, reach = [[{'bits': 0, 'lo': never, 'hi': never}]], never
    for e in sorted(events, key=lambda e: e['lo']):
        if len(groups) > 1 and e['lo'] <= reach:
            groups[-1].append(e)
        else:
            groups.append([e])
            reach = never
        reach = max(reach, e['hi'])
    return groups


def check_device_sequence(events, writes):
    """(unexplained, unchecked): of the device writes (`writes`, oldest first, all of one variable while one instance was live),
    those that no order of `events` the intervals allow explains, and those that some full order the intervals allow leaves
    unexplained (or that were not searched: a group larger than ORDER_CAP, or more than BRANCH_CAP combinations of group
    orders at once) -- each with the actions whose order decides it.

    A device write maps to one applied value: one that may have been applied by then (lo <= t + 0.05, and so may every value
    before it in the order), with every value surely applied by then (hi <= t + 0.05) at most one step after it -- the newest,
    or the one before a newest the device has not got yet -- and never an older one than the device already got.  The search
    keeps every (group, order, position, constraint on the next group's first value) still possible.  It runs once with every
    group free to take any order its intervals allow: a write it cannot place is unexplained.  Then it follows each combination
    of group orders separately -- a branch fixes a group's order when a write first reaches the group and forgets it once no
    state can go back there, and branches with the same orders and states are one -- so a write that one full order leaves
    unexplained is found even when every group taken alone has an order that explains it."""
    groups = apply_groups(events)
    big = [e.get('action_id') for g in groups if len(g) > ORDER_CAP for e in g]
    if big:
        return [], [(d, big) for d in writes]
    orders = [[p for p in itertools.permutations(range(len(g)))
               if not any(g[p[j]]['hi'] < g[p[i]]['lo'] for i in range(len(p)) for j in range(i + 1, len(p)))] for g in groups]
    min_lo = [min(e['lo'][0] for e in g) for g in groups]           # times: a device write is compared by the clock
    min_hi = [min(e['hi'][0] for e in g) for g in groups]
    unsettled = [k for k, o in enumerate(orders) if len(o) > 1]

    def reach(d):
        """The last group with a value surely applied by the time of device write d, and the last with one possibly applied."""
        c = d['t'] + 0.05
        return bisect.bisect_right(min_hi, c) - 1, bisect.bisect_right(min_lo, c) - 1

    def step(states, d, allowed):
        """The states after device write d when group k may take only the orders allowed(k)."""
        c, (last_sure, last_possible) = d['t'] + 0.05, reach(d)

        def fits(k, order, q):
            """None if d cannot be position q of group k in `order`; else the value group k + 1 must start with (-1: any)."""
            g = groups[k]
            if g[order[q]]['bits'] != d['bits'] or any(g[order[r]]['lo'][0] > c for r in range(q + 1)):
                return None                       # every value before it in this order was applied before it, so by then
            if any(g[order[r]]['hi'][0] <= c for r in range(q + 2, len(order))):
                return None
            if last_sure <= k:
                return -1
            if last_sure > k + 1 or q != len(order) - 1:
                return None
            sure = [i for i, e in enumerate(groups[k + 1]) if e['hi'][0] <= c]
            return sure[0] if len(sure) == 1 and sure[0] in {orders[k + 1][p][0] for p in allowed(k + 1)} else None

        nxt = set()
        for k0, p0, q0, first0 in states:
            for k in range(max(k0, last_sure - 1), last_possible + 1):
                for p in ([p0] if k == k0 else allowed(k)):
                    order = orders[k][p]
                    if k == k0 + 1 and first0 >= 0 and order[0] != first0:
                        continue
                    for q in range(q0 if k == k0 else 0, len(order)):
                        first = fits(k, order, q)
                        if first is not None and not (k == k0 and first0 >= 0 and first != first0):
                            nxt.add((k, p, q, first))
        return nxt

    start = frozenset({(0, 0, 0, -1)})
    states, unexplained = start, set()
    for n, d in enumerate(writes):
        nxt = step(states, d, lambda k: range(len(orders[k])))
        if nxt:
            states = nxt
        else:
            unexplained.add(n)

    branches, depends = {((), start): frozenset()}, {}       # (orders fixed, states) -> every group fixed on the way here
    for n, d in enumerate(writes):
        last_sure, last_possible = reach(d)
        split = {}
        for (fixed, states), history in branches.items():
            known, low = dict(fixed), max(min(s[0] for s in states), last_sure - 1)
            need = [k for k in unsettled if low <= k <= last_possible and k not in known]
            for choice in itertools.product(*(range(len(orders[k])) for k in need)):
                key = (tuple(sorted(fixed + tuple(zip(need, choice)))), states)
                split[key] = split.get(key, frozenset()) | history | frozenset(need)
        if len(split) > BRANCH_CAP:
            for m in range(n, len(writes)):
                if m not in unexplained:
                    depends.setdefault(m, set()).update(e.get('action_id') for k in unsettled for e in groups[k])
            break
        alive, failed = {}, set()
        for (fixed, states), history in split.items():
            known = dict(fixed)
            nxt = step(states, d, lambda k: [known[k]] if k in known else range(len(orders[k])))
            if nxt:
                low = min(s[0] for s in nxt)
                key = (tuple((k, p) for k, p in fixed if k >= low), frozenset(nxt))
                alive[key] = alive.get(key, frozenset()) | history
            else:
                failed |= history
        if n not in unexplained and failed:
            depends.setdefault(n, set()).update(e.get('action_id') for k in failed for e in groups[k])
        if alive:                              # if every branch fails here, go on as if this write were not there
            branches = alive
    return ([writes[n] for n in sorted(unexplained)],
            [(writes[n], sorted(depends[n])) for n in sorted(depends)])


class Timeline:
    """Which runtime instance was live when.  An instance starts at its lifecycle `spawned_t` (an evidenced handover: the one
    before it had ended), else at its first sighting, which comes after it started; then the one before it is known to have lived
    only until the archive last read it, and in between either may have been live.  `at` attributes every moment to one instance
    (the device check uses it); `surely` and `possibly` say how certain that is."""

    def __init__(self, starts, evidenced, last_seen):
        self.starts = sorted((t, rid) for rid, t in starts.items())     # (t, runtime_id)
        self.evidenced, self.last_seen = set(evidenced), dict(last_seen)

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

    def surely_live_until(self, rid):
        """The last moment `rid` was surely the live instance: the next instance's lifecycle start, else the last archive read
        of `rid` (not before its own start); None if no later instance was seen."""
        nxt, st = self.next_start_after(rid), self.start_of(rid)
        if nxt is None:
            return None
        if any(s == nxt and r in self.evidenced for s, r in self.starts):
            return nxt
        return max(self.last_seen.get(rid, st), st)

    def surely(self, rid, t):
        until = self.surely_live_until(rid)
        return self.at(t) == rid and (until is None or t <= until)

    def possibly(self, rid, t):
        """`rid` may have been the live instance at t: from its start -- or, without a lifecycle record, from the last moment
        the instance before it was surely live -- until the next instance's start."""
        st, nxt = self.start_of(rid), self.next_start_after(rid)
        if st is None or (nxt is not None and t >= nxt):
            return False
        if t >= st:
            return True
        prev = [r for s, r in self.starts if s < st]
        return rid not in self.evidenced and (not prev or t > self.surely_live_until(prev[-1]))


def reconcile(journal, archive, device_map, device_logs, lifecycle=None, wire=None):
    actions, jinfo = load_journal(journal)
    entries, gaps, instances, ainfo = load_archive(archive)
    if not ainfo['polls_used']:
        raise InputError('the audit archive has no usable poll')
    with open(device_map) as f:
        dmap = json.load(f)
    dev, attempts, devlogs = load_device_writes(dmap, device_logs)
    starts, evidenced, safe_states = {}, set(), []
    if lifecycle:          # an instance writes its initial outputs before it reports ready: it starts when it was spawned
        for r in read_jsonl(lifecycle):
            if r.get('ev') == 'runtime_ready' and r.get('runtime_id') and r['runtime_id'] not in evidenced:
                starts[r['runtime_id']] = r.get('spawned_t', r['t'])
                evidenced.add(r['runtime_id'])
            elif r.get('ev') == 'safe_state':
                safe_states.append(r['t'])
    sightings = [(a['runtime_id'], a['t']) for a in actions if a['runtime_id']]
    sightings += [(rid, inst['first_poll']) for rid, inst in instances.items()]
    for rid, t in sightings:       # without a lifecycle record an instance starts at its first sighting, journal or archive
        if rid not in evidenced:
            starts[rid] = min(starts.get(rid, t), t)
    tl = Timeline(starts, evidenced, {rid: inst['last_poll'] for rid, inst in instances.items()})
    posts = {}
    if wire:
        for r in read_jsonl(wire):
            if r.get('ev') == 'req' and r.get('method') == 'POST':
                posts[r.get('origin')] = posts.get(r.get('origin'), 0) + 1

    by_label = {}
    for e in entries:
        by_label.setdefault(e['entry'].get('origin'), []).append(e)
    labels = {a['label'] for a in actions}

    def device_after(var, bits, t0, t1, rid=None, log=None):
        """The first device write of `bits` to `var` from t0 to t1; with `rid`, only one that instance surely made."""
        return next((d for d in (dev if log is None else log) if d['variable'] == var and d['bits'] == bits and t0 - 0.05 <= d['t'] <= t1
                     and (rid is None or tl.surely(rid, d['t']))), None)

    def device_maybe(var, bits, t0, t1, rid):
        """A device write of `bits` to `var` from t0 to t1 that instance `rid` may have made, though not surely."""
        return next((d for d in dev if d['variable'] == var and d['bits'] == bits and t0 - 0.05 <= d['t'] <= t1
                     and tl.possibly(rid, d['t']) and not tl.surely(rid, d['t'])), None)

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

    def applied_interval(b, seen):
        """When b's write reached the VM (see Order): after its intent, before the bridge's answer -- or, with none, before the
        poll that first saw its entry.  As (time, place in the journal): two bridge records in one millisecond are in the order
        the bridge wrote them, a poll's or a device's time is compared by the clock alone."""
        lo = (b['t'], b['intent_n'])
        hi = (b['response_t'], b['response_n']) if b['transfer'] == 'response' else (seen['first_seen'], math.inf)
        return lo, max(lo, hi)

    # Every write whose label has one /audit entry with an applied value, and when it was applied.
    interval, put = {}, {}
    for b in actions:
        theirs = by_label.get(b['label'], [])
        if len(theirs) == 1 and theirs[0]['entry'].get('applied') is not None:
            interval[b['action_id']] = applied_interval(b, theirs[0])
        # what each write may have put into the VM: its applied value; nothing if denied; what it asked for if unaudited
        put[b['action_id']] = theirs[0]['entry'].get('applied') if len(theirs) == 1 else b['bits']
    # A value cannot reach a device before some write of it was applied (the outputs carry the VM's values).  When a device that
    # the timeline surely gives to the write's instance got the value after the intent, and no other write of the bridge could
    # have put that value into the variable by then, the write was applied by then.  Not for the initial, failsafe or program
    # value: something else may have written those.
    for b in actions:
        if b['action_id'] not in interval:
            continue
        lo, hi = interval[b['action_id']]
        var, v, rid = b['variable'], put[b['action_id']], by_label[b['label']][0]['runtime_id']
        m = dmap.get(var, {})
        if v in (0, m.get('failsafe', 0), m.get('program_value')):
            continue
        first = next((d for d in dev if d['variable'] == var and d['bits'] == v and lo[0] <= d['t'] < hi[0]
                      and tl.surely(rid, d['t'])), None)
        if first and not any(c is not b and c['variable'] == var and put[c['action_id']] == v and c['t'] <= first['t']
                             for c in actions):
            interval[b['action_id']] = (lo, (first['t'], math.inf))

    # Writes /audit shows applied: the label's one entry matches its intent, records an applied value and agrees with the bridge's
    # HTTP answer, if any (an applied write whose client left has no answer, and still counts).  Per (instance, variable).
    applied_writes = {}
    for b in actions:
        theirs = by_label.get(b['label'], [])
        e = theirs[0]['entry'] if len(theirs) == 1 else {}
        applied = e.get('applied')
        if (applied is None or e.get('name') != b['variable'] or e.get('requested') != b['bits']
                or b['http_status'] not in (None, 200)):
            continue
        lo, hi = interval[b['action_id']]
        applied_writes.setdefault((theirs[0]['runtime_id'], b['variable']), []).append(
            {'action_id': b['action_id'], 'bits': applied, 'lo': lo, 'hi': hi})

    def rivals(rid, var, applied, lo, hi):
        """The writes in instance `rid` that applied another value to `var` than a write applied in [lo, hi]: of those known to
        come after it, the one surely applied first (or None); and those whose order with it is unknown.  The same value again
        does not end the duty to deliver it."""
        other = [c for c in applied_writes.get((rid, var), []) if c['bits'] != applied]
        later = [c for c in other if c['lo'] > hi]
        unordered = sorted(c['action_id'] for c in other if not (c['lo'] > hi or c['hi'] < lo))
        return (min(later, key=lambda c: c['hi']) if later else None), unordered

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
                inst = mine[0]['runtime_id']
                lo_key, hi_key = interval.get(a['action_id']) or applied_interval(a, mine[0])   # two entries: unexplained anyway
                lo = a['t']
                set_events.append({'variable': var, 'bits': applied, 'lo': lo_key, 'hi': hi_key, 'runtime_id': inst,
                                   'action_id': a['action_id']})
                # The value must reach the device until a value known to be applied after it, or the end of its instance, takes
                # over.  One that waits for a link (undelivered, or a device IA2 still believes healthy) arrives when the link
                # returns.  Only a device write this instance surely made counts: the next instance writing the same value
                # delivers nothing for this one.
                succ, unordered = rivals(inst, var, applied, lo_key, hi_key)
                ends, live = tl.next_start_after(inst), tl.surely_live_until(inst)
                stops = ([(succ['hi'][0], succ['action_id'])] if succ else []) + ([(ends, 'restart')] if ends is not None and ends > lo else [])
                end, replaced_by = min(stops) if stops else (None, None)
                until = end if end is not None else log.end
                look = until + DEVICE_SLACK_S if end is not None else until    # the instance's own writes only: see device_after
                sure_until = until if live is None else min(until, live)
                seen = device_after(var, applied, lo, look, inst)
                prior = device_write_before(var, lo)
                row['device'] = 'seen' if seen else 'not_seen'
                if seen:
                    pass
                elif prior and prior['bits'] == applied and tl.surely(inst, prior['t']) and log.covers(prior['t'], lo):
                    row['device'] = 'already_held'
                elif device_after(var, applied, lo, look, inst, attempts):
                    row['device'] = 'sent_unanswered'
                elif 'not delivered' in result:
                    row['device'] = 'undelivered'
                elif end is not None and end <= lo + DEVICE_SLACK_S:
                    row['device'], row['replaced_by'] = 'replaced_before_delivery', replaced_by   # one output cycle: merged
                elif end is not None and log.no_link(lo, end):
                    # IA2 had not noticed yet that the link was gone (it answered 200, nothing about delivery); the device's own
                    # log shows no working link from the write until a newer value or the end of the instance replaced it
                    row['device'], row['replaced_by'] = 'link_down_until_replaced', replaced_by
                elif end is None and log.complete and log.no_link(lo, log.end):
                    row['device'] = 'link_down_until_log_end'
                elif unordered:
                    # another value may have replaced it within the output cycle, or have come first: the order decides
                    gap = row['device'] = 'uncheckable_order'
                    row['unordered_with'] = unordered
                elif device_maybe(var, applied, lo, look, inst) or sure_until <= lo + DEVICE_SLACK_S < until:
                    # the device got it, or the instance ended, while it is unknown which instance was live
                    gap = row['device'] = 'uncheckable_order'
                    row['handover'] = [live, ends]
                elif log.covers(lo, sure_until) and log.answered(lo, sure_until):
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
            if seen:                        # applied after its intent and before the device got it
                lo_key = (a['t'], a['intent_n'])
                event = {'variable': var, 'bits': bits, 'lo': lo_key, 'hi': max(lo_key, (seen['t'], math.inf)),
                         'runtime_id': a['runtime_id'], 'action_id': a['action_id']}
            if wire and row['posts'] == 0 and not seen:
                row['class'] = 'not_sent'          # the proxy is the evidence; the archive's coverage does not matter
            elif not ok:
                row['class'] = why
                if seen:
                    set_events.append(event)
            elif seen and row['device'] == 'seen':
                set_events.append(event)
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

    # Each variable's device writes while an instance was live must follow one order of the values the bridge had that instance
    # put into it -- every order the apply intervals allow (check_device_sequence), not the order of /audit's ring.
    sequences, unowned, unexplained_dev, unchecked_dev = {}, {}, [], []
    for d in dev:
        m = dmap[d['variable']]
        owners = [r for _, r in tl.starts if tl.possibly(r, d['t'])] or [tl.at(d['t'])]
        if 'program_value' in m and d['bits'] == m['program_value']:
            continue
        if d['bits'] == m.get('failsafe', 0) and any(ts - 0.05 <= d['t'] and tl.at(ts) in owners for ts in safe_states):
            continue
        sure = [r for r in owners if tl.surely(r, d['t'])]
        if sure or len(owners) == 1:
            sequences.setdefault((sure[0] if sure else owners[0], d['variable']), []).append(d)
        else:
            unowned.setdefault(tuple(owners), []).append(d)

    def applied_by(rid, var):
        return [s for s in set_events if s['variable'] == var and s['runtime_id'] == rid]

    settled = {}
    for (rid, var), writes in sequences.items():
        bad, unchecked = check_device_sequence(applied_by(rid, var), writes)
        settled[rid, var] = ({id(d) for d in bad}, {id(d) for d, _ in unchecked})
        values = sorted({0} | {s['bits'] for s in applied_by(rid, var)})
        unexplained_dev += [dict(d, runtime_id=rid, applied_values=values) for d in bad]
        unchecked_dev += [dict(d, runtime_id=rid, order_decided_by=ids) for d, ids in unchecked]

    # A handover nobody recorded happened at one moment, for every output at once: the device writes made while it is unknown which
    # instance was live are split, in time order, between the instances that may have made them -- every such split is tried.
    # One is explained when each instance's writes, its own and the ones the split gives it, are explained by every order of its
    # values without making any of its own writes worse.  If every split is, nothing is reported; if none is, the writes no split
    # explains (or, if each fails only under some splits, all of them together) are unexplained; otherwise they are unchecked.
    for owners, writes in unowned.items():
        if math.comb(len(writes) + len(owners) - 1, len(writes)) > HANDOVER_CAP:
            unchecked_dev += [dict(d, runtime_id=None, possible_instances=list(owners)) for d in writes]
            continue
        outcomes, blamed = [], []
        for split in itertools.combinations_with_replacement(range(len(owners)), len(writes)):
            given = {}
            for d, i in zip(writes, split):
                given.setdefault((owners[i], d['variable']), []).append(d)
            outcome, bad_here = 'explained', set()
            for (rid, var), extra in given.items():
                was_bad, was_unchecked = settled.get((rid, var), (set(), set()))
                bad, unchecked = check_device_sequence(applied_by(rid, var), sorted(sequences.get((rid, var), []) + extra,
                                                                                      key=lambda w: w['t']))
                newly_bad = {id(w) for w in bad} - was_bad
                bad_here |= newly_bad
                if newly_bad:
                    outcome = 'unexplained'
                elif {id(w) for w, _ in unchecked} - was_unchecked - was_bad and outcome == 'explained':
                    outcome = 'partly'
            outcomes.append(outcome)
            blamed.append(bad_here)
        if all(o == 'explained' for o in outcomes):
            continue
        ids = {id(d) for d in writes}
        culprits = set.intersection(*blamed) & ids if all(o == 'unexplained' for o in outcomes) else set()
        if all(o == 'unexplained' for o in outcomes) and not culprits:
            culprits = ids                       # each could be someone's, but no single handover makes them all so
        for d in writes:
            (unexplained_dev if id(d) in culprits else unchecked_dev).append(dict(d, runtime_id=None, possible_instances=list(owners)))
    unexplained_dev.sort(key=lambda d: d['t'])
    unchecked_dev.sort(key=lambda d: d['t'])

    counts = {}
    for r in rows:
        counts[r['class']] = counts.get(r['class'], 0) + 1
    discrepancy = unknown or unexplained_dev or counts.get('unexplained') or not jinfo['chain_ok']
    incomplete = any(counts.get(c) for c in EVIDENCE_GAPS) or unchecked_dev
    return {
        'verdict': 'DISCREPANCY' if discrepancy else 'INCOMPLETE' if incomplete else 'CLEAN',
        'counts': dict(sorted(counts.items()), intents=len(rows), unknown_writes=len(unknown),
                       unexplained_device_writes=len(unexplained_dev), unchecked_device_writes=len(unchecked_dev),
                       device_writes=len(dev)),
        'journal': jinfo,
        'audit_archive': dict(ainfo, entries=len(entries), turnover_gaps=gaps,
                              instances=sorted(instances.values(), key=lambda i: i['first_poll'])),
        'device_logs': {role: log.summary() for role, log in sorted(devlogs.items())},
        'unknown_writes': unknown,
        'unexplained': [r for r in rows if r['class'] == 'unexplained'],
        'unexplained_device_writes': unexplained_dev,
        'unchecked_device_writes': unchecked_dev,
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
        print('reconcile_report: evidence gap: %s' % ', '.join('%d %s' % (report['counts'][c], c) for c in
                                                                EVIDENCE_GAPS + ('unchecked_device_writes',)
                                                                if report['counts'].get(c)), file=sys.stderr)
    return {'CLEAN': 0, 'DISCREPANCY': 1, 'INCOMPLETE': 2}[report['verdict']]


if __name__ == '__main__':
    sys.exit(main())
