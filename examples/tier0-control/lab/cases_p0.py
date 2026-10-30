"""The remaining P0 cases of TEST-CASES.md (the slice cases A1, B1, B3, C1 live in harness.py).

Group 1: A2 B2 B4 B6 B7 B8 B9 B11          value handling, throttle/time/dedup, stubbed IA2 faults
Group 2: C3 C4 C6 C8 E1 E2 E3              delivery, silent links, program-owned outputs, restarts, read-side verification
Group 3: R1 R2 R3 F1 F2                    crash recovery, a stopped runtime, tool surface, forged requests
         R3T1 R3T1K R3T1X R3T2             R3 timings: read and queued, its control and crash variant, applied but unanswered

Each case is `async def case_X(st, ck, notes)` and registers in CASES as (fn, registry targets[, options]).
Expectations are the TEST-CASES.md EXPECT/pass columns; where the implementation deliberately differs, the check says so."""
import asyncio
import hashlib
import inspect
import json
import os
import signal
import struct
import subprocess
import sys
import time

import harness
import verifier
from harness import (BRIDGE_RUN, ROOT, Checks, now_ms, sha256_file, verify_chain, wait_until, wire_summary)  # noqa: F401

HOOKS_SYNC = os.path.join(ROOT, 'lab', 'hooks_sync.py')
READONLY_SCHEMA = os.path.join(ROOT, 'tests', 'fixtures', 'readonly-tools.schema.json')


# ---- registry builders: the rate limits are opened up so a case can send several requests to one key ---------------------
def tgt(key, var, typ, lo, hi, verify=None, **kw):
    t = harness.target(key, var, typ, lo, hi, verify=verify, **kw)
    t.setdefault('min_interval_ms', 0)
    t.setdefault('max_rate_per_min', 60)
    return t


def fb(feedback, tolerance=0.05, settle_ms=5000):
    return {'feedback': feedback, 'tolerance': tolerance, 'settle_ms': settle_ms}


FLOW = tgt('flow_setpoint', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], verify=fb('sp_flow_fb'))
FLOW_WIDE = tgt('flow_wide', 'sp_flow', 'REAL', 0.0, 50.0, require_devices=['coupler'], verify=fb('sp_flow_fb', 10.0))
FLOW_FAST = tgt('flow_fast', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], verify=fb('sp_flow_fb', 0.05, 1500))
FLOW_NOFB = tgt('flow_nofb', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'])
BIG = tgt('big_setpoint', 'big', 'DINT', -1000, 1000, require_devices=['coupler'], verify=fb('big_fb', 0.0))
LAMP = tgt('lamp', 'lamp', 'BOOL', 0, 1, require_devices=['coupler'], verify=fb('lamp_fb', 0.0))
PROG = tgt('prog_setpoint', 'prog_sp', 'REAL', 0.0, 10.0, require_devices=['coupler'], verify=fb('prog_fb', 0.05, 3000))
D2 = tgt('d2_set', 'd2_sp', 'REAL', 0.0, 40.0, require_devices=[], require_fieldbus_healthy=False, verify=fb('d2_fb', 0.05, 3000))


def f32bits(x):
    return struct.unpack('>i', struct.pack('>f', x))[0]


def f32(x):
    return struct.unpack('>f', struct.pack('>f', x))[0]


# ---- small helpers -------------------------------------------------------------------------------------------------------
async def submit_wait(st, key, value, ttl_ms=20000, timeout=15, **override):
    p = st.standin.submit(key, value, ttl_ms=ttl_ms, **override)
    ev = await wait_until(lambda: st.event(p['action_id']), timeout)
    return p, ev


async def wait_deadline(p, extra=0.15):
    await asyncio.sleep(max(0.0, p['deadline_ms'] / 1000.0 - time.time()) + extra)


async def derive_until_settled(st, p, key, timeout=10):
    res = None
    end = time.time() + timeout
    while time.time() < end:
        res = verifier.derive(p, st.entry(key), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
        if res['status'] != 'accepted':
            break
        await asyncio.sleep(0.2)
    return res


def posts(st):
    return [r for r in st.wire() if r['method'] == 'POST']


async def audit_list(st):
    s, body = await st.rt('GET', '/audit')
    return body if isinstance(body, list) else []


def short(ev):
    return None if ev is None else (ev['status'], ev.get('reason'))


async def healthy(st, device, want, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        s, h = await st.rt('GET', '/health', timeout=1.0)
        row = next((d for d in (h or {}).get('devices', []) if isinstance(h, dict) and d.get('name') == device), None) if isinstance(h, dict) else None
        if row is not None and bool(row.get('healthy')) == want:
            return time.time()
        await asyncio.sleep(0.02)
    return None


def vars_of(status_body):
    return {v['name']: v for v in (status_body or {}).get('last_snapshot', {}).get('vars', [])}


# ======================================================================================================================
# Group 1
# ======================================================================================================================
async def case_A2(st, ck, notes):
    """REAL / DINT / BOOL at min, max, middle and rounding-sensitive values; out-of-range and wrong-typed values; an
    unsupported (LREAL) target.  Read-back is compared bit for bit."""
    await asyncio.sleep(1.0)
    s1 = st.slave
    sent_ok = []

    async def good(key, value, bits, words=None, coil=None, echo=None):
        p, ev = await submit_wait(st, key, value, ttl_ms=1000)
        ck('%s <- %r: accepted with bits %d' % (key, value, bits),
           ev is not None and ev['status'] == 'accepted' and ev.get('applied_bits') == bits, short(ev) if ev else None)
        sent_ok.append(p['action_id'])
        await asyncio.sleep(0.5)                                          # IA2 delivers, then polls the echo back
        if words is not None:
            ck('%s <- %r: the device holds words %s' % (key, value, ['%04x' % w for w in words[1]]),
               [s1.regs.get(words[0]), s1.regs.get(words[0] + 1)] == words[1], [s1.regs.get(words[0]), s1.regs.get(words[0] + 1)])
        if coil is not None:
            ck('%s <- %r: the device coil is %d' % (key, value, coil), s1.coils.get(0, 0) == coil, s1.coils)
        if echo is not None:
            sc, status = await st.rt('GET', '/status')
            row = vars_of(status).get(echo[0])
            ck('%s <- %r: read-back variable %s has the same bits' % (key, value, echo[0]),
               row is not None and row['bits'] == echo[1], row)
        await wait_deadline(p)

    for x in (0.0, 40.0, 20.5, 12.3456789, 39.99999999, 1e-30):
        bits = f32bits(f32(x))
        u = bits & 0xFFFFFFFF
        await good('flow_setpoint', x, bits, words=(100, [u >> 16, u & 0xFFFF]), echo=('sp_flow_fb', u))
    for v in (-1000, 1000, 0, 777):
        u = v & 0xFFFFFFFF
        sc, status = await st.rt('GET', '/status')
        await good('big_setpoint', v, v, words=(120, [u >> 16, u & 0xFFFF]))
        sc, status = await st.rt('GET', '/status')
        row = vars_of(status).get('big_fb')
        ck('big_setpoint <- %d: read-back value is %d' % (v, v), row is not None and row['value'] == str(v), row)
    for v, want in ((1, 1), (0, 0), (True, 1)):
        await good('lamp', v, want, coil=want, echo=('lamp_fb', want))

    expect = []

    def bad(key, value, reason, label):
        p = st.standin.submit(key, value, ttl_ms=20000)
        expect.append((label, p['action_id'], reason))
    bad('flow_setpoint', -0.001, 'out_of_range', 'REAL min - epsilon')
    bad('flow_setpoint', 40.001, 'out_of_range', 'REAL max + epsilon')
    bad('big_setpoint', -1001, 'out_of_range', 'DINT min - 1')
    bad('big_setpoint', 1001, 'out_of_range', 'DINT max + 1')
    bad('big_setpoint', 1.5, 'bad_value', 'DINT 1.5')
    bad('lamp', 2, 'bad_value', 'BOOL 2')
    bad('lamp', -1, 'bad_value', 'BOOL -1')
    bad('lamp', 0.5, 'bad_value', 'BOOL 0.5')
    await wait_until(lambda: all(st.event(a) for _, a, _ in expect), 15)
    for label, aid, reason in expect:
        ev = st.event(aid)
        ck('%s -> rejected/%s' % (label, reason), ev is not None and short(ev) == ('rejected', reason), short(ev))
    await asyncio.sleep(0.5)
    w = wire_summary(st.wire())
    ck('only the %d accepted writes reached IA2 (rejected values left no trace on the wire)' % len(sent_ok),
       len(w['posts']) == len(sent_ok), w)
    audit = await audit_list(st)
    ck('IA2 /audit has exactly %d entries' % len(sent_ok), len(audit) == len(sent_ok), len(audit))
    ck('no write was reported as modified or clamped', all(e.get('result') == 'ok' for e in audit), [e.get('result') for e in audit if e.get('result') != 'ok'])
    # an LREAL target is not supported in v0: the registry itself is refused, so the bridge never starts
    lreal = json.dumps({'version': 1, 'targets': [dict(FLOW, key='lr', type='LREAL')]})
    wire0 = len(st.wire())
    rc, err = st.run_bridge_once(lreal, 'lreal')
    ck('a registry with an LREAL target is refused at startup (exit 2)', rc == 2 and 'type' in err, (rc, err[:200]))
    ck('...and the refused start sent nothing to IA2', len(st.wire()) == wire0)
    notes['deviation'] = ('TEST-CASES A2 expects an LREAL target to be `rejected`; the registry loader refuses the whole registry instead '
                          '(fail closed at load), so the request is never even considered.')


NAN_SENTINEL = 987654321.5


async def case_B2(st, ck, notes):
    """Values no gate may let through.  NaN / Infinity literals are not strict JSON, so the payload is refused before it can be
    attributed to an action (journal only); everything else is a normal `rejected` with a reason."""
    await asyncio.sleep(1.0)
    expect, raw_expect = [], []

    def send(label, key, value, reason, token=None):
        p = st.standin.payload(key, value)
        text = st.standin.payload_text(p)
        if token is not None:
            text = text.replace(repr(NAN_SENTINEL), token)
            assert token in text
        st.standin.submit_text(text)
        if token in ('NaN', 'Infinity', '-Infinity'):
            raw_expect.append((label, hashlib.sha256(text.encode()).hexdigest()))
        else:
            expect.append((label, p['action_id'], reason))
    send('NaN literal', 'flow_setpoint', NAN_SENTINEL, None, token='NaN')
    send('Infinity literal', 'flow_setpoint', NAN_SENTINEL, None, token='Infinity')
    send('-Infinity literal', 'flow_setpoint', NAN_SENTINEL, None, token='-Infinity')
    send('1e999 (parses as +inf)', 'flow_setpoint', NAN_SENTINEL, 'bad_value', token='1e999')
    send('-1e999 (parses as -inf)', 'flow_setpoint', NAN_SENTINEL, 'bad_value', token='-1e999')
    send('string "12.5"', 'flow_setpoint', '12.5', 'bad_request')
    send('null', 'flow_setpoint', None, 'bad_request')
    send('true for a REAL', 'flow_setpoint', True, 'bad_value')
    send('huge integer 10**30', 'flow_setpoint', 10 ** 30, 'out_of_range')
    send('REAL above max (40.5)', 'flow_setpoint', 40.5, 'out_of_range')
    send('negative zero', 'flow_setpoint', -0.0, 'bad_value')
    send('BOOL = 2', 'lamp', 2, 'bad_value')
    send('BOOL = -1', 'lamp', -1, 'bad_value')
    send('BOOL = "true"', 'lamp', 'true', 'bad_request')
    await wait_until(lambda: all(st.event(a) for _, a, _ in expect), 15)
    for label, aid, reason in expect:
        ev = st.event(aid)
        ck('%s -> rejected/%s' % (label, reason), ev is not None and short(ev) == ('rejected', reason), short(ev))
    await asyncio.sleep(1.0)
    journal = st.journal()
    for label, h in raw_expect:
        rec = next((r for r in journal if r['kind'] == 'gate_reject' and r.get('req_hash') == h), None)
        ck('%s: refused as non-strict JSON (journal only, bad_request)' % label,
           rec is not None and rec.get('action_id') is None and rec.get('reason') == 'bad_request', rec and rec.get('detail'))
    ck('NOTHING reached IA2 (wire log empty)', len(st.wire()) == 0, wire_summary(st.wire()))
    ck('IA2 /audit is empty', await audit_list(st) == [])
    ck('the device never received a non-boot write', all(e.get('value') in (0, None) for e in st.slave_writes(100)))
    ck('the bridge is still running', st.bridge.returncode is None)
    st.standin.append_line('x' * 70000)                                    # W8: a line over the 64 KiB limit is not read, only recorded
    rec = await wait_until(lambda: next((r for r in st.journal() if r['kind'] == 'gate_reject' and 'exceeds the limit' in str(r.get('detail'))), None), 8)
    ck('an oversize line is recorded once as a bad_request (length in the detail) and the bridge carries on', rec is not None and '70000' in rec['detail']
       and st.bridge.returncode is None, rec and rec.get('detail'))
    notes['deviation'] = ('TEST-CASES B2 expects NaN/inf to be refused by G3; NaN / Infinity LITERALS never get that far (G1 refuses them as '
                          'non-strict JSON and, with no trustworthy action_id, answers in the journal only); 1e999 does reach G3 as bad_value.')


async def case_B4(st, ck, notes):
    """Registry range wider than IA2's governance: IA2 clamps; the bridge says `modified`; no feedback can make it `verified`."""
    await asyncio.sleep(1.0)
    p, ev = await submit_wait(st, 'flow_wide', 45.0, ttl_ms=20000)
    ck('the bridge answered', ev is not None)
    if ev is None:
        return
    ck('status modified / clamped', short(ev) == ('modified', 'clamped'), short(ev))
    ck('applied value is 40.0 (IA2 clamp), not the requested 45.0', ev.get('applied_value') == 40.0 and ev.get('requested_value') == 45.0,
       (ev.get('applied_value'), ev.get('requested_value')))
    await asyncio.sleep(1.5)                                               # the echo reads 40.0, inside the registry tolerance of 10
    sc, status = await st.rt('GET', '/status')
    row = vars_of(status).get('sp_flow_fb')
    ck('the feedback really does show 40.0', row is not None and row['value'] in ('40', '40.0'), row)
    res = verifier.derive(p, st.entry('flow_wide'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('the verifier keeps it `modified`', res['status'] == 'modified' and res.get('applied_value') == 40.0, res)
    control = verifier.derive(p, st.entry('flow_wide'), [st.signed_event(dict(ev, status='accepted'))], st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('negative control: had it been `accepted`, the same feedback would have verified it', control['status'] == 'verified', control)
    ck('no `verified` in the trace', all(e.get('status') != 'verified' for e in st.trace()))
    audit = await audit_list(st)
    ck('IA2 /audit: one entry, `clamped`, requested 45.0 applied 40.0',
       len(audit) == 1 and audit[0].get('result') == 'clamped' and audit[0].get('requested') == f32bits(45.0) and audit[0].get('applied') == f32bits(40.0), audit)
    await wait_deadline(p, 1.5)
    res = verifier.derive(p, st.entry('flow_wide'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('still `modified` after the deadline', res['status'] == 'modified', res)


async def case_B6(st, ck, notes):
    await asyncio.sleep(1.0)
    t = now_ms()
    expect = []
    for label, kw in (('deadline already past', dict(ttl_ms=-1000)),
                      ('deadline beyond max_deadline_ms', dict(ttl_ms=120000)),
                      ('requested_at 3 s in the future', dict(requested_at_ms=t + 3000)),
                      ('requested_at 3 s in the past', dict(requested_at_ms=t - 3000))):
        p = st.standin.submit('flow_setpoint', 12.5, **kw)
        expect.append((label, p['action_id']))
    await wait_until(lambda: all(st.event(a) for _, a in expect), 15)
    for label, aid in expect:
        ev = st.event(aid)
        ck('%s -> rejected/expired' % label, ev is not None and short(ev) == ('rejected', 'expired'), short(ev))
    p, ev = await submit_wait(st, 'flow_setpoint', 12.5, ttl_ms=5000, requested_at_ms=now_ms() + 1500)
    ck('control: 1.5 s of clock skew is tolerated', ev is not None and ev['status'] == 'accepted', short(ev))
    w = wire_summary(st.wire())
    ck('only the control request reached IA2', w['posts'] == ['/write'], w)
    audit = await audit_list(st)
    ck('IA2 /audit: only the control request', len(audit) == 1, len(audit))


async def case_B7(st, ck, notes):
    await asyncio.sleep(1.0)
    p = st.standin.payload('flow_setpoint', 12.5, ttl_ms=2500)
    line = st.standin.envelope(st.standin.payload_text(p))
    aid = p['action_id']
    for _ in range(3):
        st.standin.append_line(line)
        await asyncio.sleep(0.3)
    ev = await wait_until(lambda: st.event(aid), 10)
    await wait_deadline(p, 0.3)
    st.standin.append_line(line)                                          # a fourth, after the deadline
    await asyncio.sleep(1.0)
    ck('the answer is `accepted`', ev is not None and ev['status'] == 'accepted', short(ev))
    same = [e for e in st.trace() if e.get('action_id') == aid]
    ck('exactly one trace event for the action (same event_id every time)', len(same) == 1, len(same))
    ck('journal has one intent for the action', len([r for r in st.journal() if r['kind'] == 'intent' and r.get('action_id') == aid]) == 1)
    ck('exactly one POST /write reached IA2', len(posts(st)) == 1, wire_summary(st.wire()))
    audit = await audit_list(st)
    ck('IA2 /audit has exactly one entry', len(audit) == 1, len(audit))
    ck('the repeats left no conflict record', not any(r['kind'] == 'conflict' for r in st.journal()))
    kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
    ck('the journal holds exactly intent, response, post_check, terminal for the action: the repeats wrote NOTHING', kinds == ['intent', 'response', 'post_check', 'terminal'], kinds)


async def case_B8(st, ck, notes):
    await asyncio.sleep(1.0)
    p1, ev1 = await submit_wait(st, 'flow_setpoint', 12.5, ttl_ms=20000)
    ck('the first request is accepted', ev1 is not None and ev1['status'] == 'accepted', short(ev1))
    p2 = dict(p1, target=13.0, event_id='another-event')                  # same action_id, different content, validly signed
    st.standin.submit_text(st.standin.payload_text(p2))
    notice = await wait_until(lambda: next((e for e in st.trace() if e.get('kind') == 'conflict_notice' and e.get('action_id') == p1['action_id']), None), 10)
    ck('a conflict notice with reason conflicting_action_id was written', notice is not None and notice.get('reason') == 'conflicting_action_id'
       and notice.get('status') == 'rejected', notice)
    await asyncio.sleep(1.0)
    ck('the original action is unchanged', short(st.event(p1['action_id'])) == ('accepted', None), short(st.event(p1['action_id'])))
    ck('the original action is still verifiable (the notice did not disturb it)',
       (await derive_until_settled(st, p1, 'flow_setpoint'))['status'] == 'verified')
    ck('no second request reached IA2', len(posts(st)) == 1, wire_summary(st.wire()))
    audit = await audit_list(st)
    ck('IA2 /audit has exactly one entry (the original)', len(audit) == 1 and audit[0].get('requested') == f32bits(12.5), audit)
    ck('journal records the conflict', any(r['kind'] == 'conflict' for r in st.journal()))


async def case_B9(st, ck, notes):
    await asyncio.sleep(1.0)
    p = st.standin.payload('flow_setpoint', 12.5, ttl_ms=30000)
    line = st.standin.envelope(st.standin.payload_text(p))
    aid = p['action_id']
    st.standin.append_line(line)
    ev = await wait_until(lambda: st.event(aid), 10)
    ck('accepted before the restart', ev is not None and ev['status'] == 'accepted', short(ev))
    await st.stop_bridge()
    info = await st.start_bridge()                                         # the new bridge re-reads the requests file from the start
    st.standin.append_line(line)                                          # and the same request arrives again
    await asyncio.sleep(2.5)
    ck('the restarted bridge replayed from the same journal', True, info)
    ck('journal has two `start` records and one intent', len([r for r in st.journal() if r['kind'] == 'start']) == 2
       and len([r for r in st.journal() if r['kind'] == 'intent']) == 1)
    ck('exactly one POST /write across both bridge lifetimes', len(posts(st)) == 1, wire_summary(st.wire()))
    audit = await audit_list(st)
    ck('IA2 /audit has exactly one entry', len(audit) == 1, len(audit))
    ck('one trace event, unchanged', len([e for e in st.trace() if e.get('action_id') == aid]) == 1 and short(st.event(aid)) == ('accepted', None))
    kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
    ck('the journal holds exactly intent, response, post_check, terminal for the action: the replays wrote NOTHING', kinds == ['intent', 'response', 'post_check', 'terminal'], kinds)
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies across the restart', ok, n)


async def case_B11(st, ck, notes):
    """The bridge against a stub that misbehaves in every way a runtime can.  Nothing but the control may become `accepted`."""
    stub = st.stub_srv
    bits = f32bits(12.5)
    ok_body = {'ok': True, 'name': 'sp_flow', 'value': bits, 'undelivered_device': None}
    script = [
        ('HTTP 500', {'kind': 'http', 'status': 500, 'body': 'boom'}, ('unknown', 'vm_error')),
        ('200 with a non-JSON body', {'kind': 'http', 'status': 200, 'body': 'not json at all'}, ('unknown', 'vm_error')),
        ('200 without undelivered_device', {'kind': 'http', 'status': 200, 'ctype': 'application/json',
                                              'body': json.dumps({k: v for k, v in ok_body.items() if k != 'undelivered_device'})}, ('unknown', 'vm_error')),
        ('200 without value', {'kind': 'http', 'status': 200, 'ctype': 'application/json',
                                'body': json.dumps({k: v for k, v in ok_body.items() if k != 'value'})}, ('unknown', 'vm_error')),
        ('302 redirect', {'kind': 'http', 'status': 302, 'headers': {'Location': '/elsewhere'}, 'body': ''}, ('unknown', 'vm_error')),
        ('200 with an empty body', {'kind': 'http', 'status': 200, 'body': ''}, ('unknown', 'vm_error')),
        ('body cut off, connection closed', {'kind': 'half'}, ('unknown', 'runtime_unreachable')),
        ('no answer at all (6 s; bridge timeout is 3 s)', {'kind': 'hang', 'seconds': 6}, ('unknown', 'runtime_unreachable')),
        ('control: a well-formed 200', {'kind': 'ok'}, ('accepted', None)),
    ]
    stub.script(*[b for _, b, _ in script])
    results = []
    for label, _, want in script:
        p, ev = await submit_wait(st, 'flow_setpoint', 12.5, ttl_ms=20000, timeout=25)
        results.append((label, ev, want))
        ck('%s -> %s' % (label, '/'.join(str(x) for x in want)), ev is not None and short(ev) == want, short(ev))
    ck('only the control became `accepted`', [short(e) for _, e, _ in results if e and e['status'] == 'accepted'] == [('accepted', None)])
    ck('exactly one POST per request: no retry', len([r for r in stub.requests if r['method'] == 'POST']) == len(script),
       len([r for r in stub.requests if r['method'] == 'POST']))
    ck('the redirect was not followed', not any(r['path'] == '/elsewhere' for r in stub.requests))
    def notes_now():
        return [e for e in st.trace() if e.get('kind') == 'reconcile_note']
    await wait_until(lambda: len(notes_now()) >= 16, 25)                     # the periodic reconcile looks twice at each unknown
    notes['reconcile_notes'] = [{'pass': e.get('pass_no'), 'landed': e.get('landed'), 'audit_entry': e.get('audit_entry'), 'final': e.get('final')}
                                for e in notes_now()]
    ck('each of the 8 unknowns got two reconcile notes, the second one final (stub /audit is empty: entry absent, landed not claimed either way)',
       len(notes_now()) == 16 and all(e.get('audit_entry') == 'absent' and e.get('landed') is None for e in notes_now())
       and sorted((e.get('pass_no'), e.get('final')) for e in notes_now()) == [(1, False)] * 8 + [(2, True)] * 8, notes['reconcile_notes'])
    after = {x['action_id']: short(x) for x in st.trace() if x.get('kind') == 'action_status'}
    ck('...and no action changed status because of it', sorted(map(str, after.values())) == sorted(str(w) for _, _, w in results), after)


SILENT = [tgt('silent_%02d' % i, 'd2_sp', 'REAL', 0.0, 40.0, require_devices=['second'], require_fieldbus_healthy=False, verify=fb('d2_fb', 0.05, 3000)) for i in range(30)]

# ======================================================================================================================
# Group 2
# ======================================================================================================================
async def case_C3(st, ck, notes):
    """A write while a device link is down: IA2 accepts it into the VM, reports the device, and delivers it LATE, when the link
    returns.  D1(a): the bridge does not retract.  The timings are the evidence for revisiting D1."""
    await asyncio.sleep(1.0)
    s2 = st.slave2
    runs = []
    for mode in ('close', 'refuse'):
        await s2.set_mode(mode)
        t_down = await healthy(st, 'second', False, timeout=20)
        ck('[%s] the runtime flagged `second` unhealthy' % mode, t_down is not None)
        p, ev = await submit_wait(st, 'd2_set', 5.0, ttl_ms=4000)
        ck('[%s] status undelivered / link_down, naming the device' % mode,
           ev is not None and short(ev) == ('undelivered', 'link_down') and ev.get('undelivered_device') == 'second', ev and (short(ev), ev.get('undelivered_device')))
        audit = await audit_list(st)
        mine = [e for e in audit if e.get('origin') == (ev or {}).get('label')]
        ck('[%s] IA2 /audit text says the value was not delivered (link down)' % mode,
           len(mine) == 1 and 'not delivered' in str(mine[0].get('result')) and 'link down' in str(mine[0].get('result')), mine)
        await wait_deadline(p, 0.3)
        ck('[%s] nothing reached the device while the link was down' % mode,
           not [e for e in st.slave_writes(100, p['requested_at_ms'] / 1000.0 - 1, slave=2) if e.get('value') == 0x40A0])
        t_restore = time.time()
        await s2.set_mode('normal')
        arrived = await wait_until(lambda: next(iter([e for e in st.slave_writes(100, t_restore, slave=2) if e.get('value') == 0x40A0]), None), 25, 0.05)
        ck('[%s] the write was delivered LATE, after the link returned (D1(a) does not retract it)' % mode, arrived is not None)
        rec = {'mode': mode, 'seconds_from_restore_to_arrival': None if arrived is None else round(arrived['t_req'] - t_restore, 3),
               'seconds_after_deadline': None if arrived is None else round(arrived['t_req'] - p['deadline_ms'] / 1000.0, 3),
               'request_to_arrival_s': None if arrived is None else round(arrived['t_req'] - p['requested_at_ms'] / 1000.0, 3),
               'flip_after_restart_s': None, 'value_that_arrived': '0x%04x%04x' % (s2.regs.get(100, 0), s2.regs.get(101, 0))}
        runs.append(rec)
        ck('[%s] the value that arrived is the requested 5.0 (0x40a00000)' % mode, rec['value_that_arrived'] == '0x40a00000', rec['value_that_arrived'])
        await asyncio.sleep(1.5)
        audit2 = await audit_list(st)
        ck('[%s] the late delivery added NO entry to IA2 /audit' % mode, len([e for e in audit2 if e.get('origin') == (ev or {}).get('label')]) == 1)
        res = verifier.derive(p, st.entry('d2_set'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
        ck('[%s] even with the matching feedback now present, the action stays `undelivered`' % mode, res['status'] == 'undelivered', res)
        ok = await healthy(st, 'second', True, timeout=30)
        ck('[%s] the device is healthy again' % mode, ok is not None)
        await asyncio.sleep(1.0)
    notes['D1_evidence'] = runs
    ck('the bridge never wrote a second request (no retraction)', len(posts(st)) == 2, wire_summary(st.wire()))


async def case_C4(st, ck, notes):
    """A slave that stops answering but keeps its connection: the runtime keeps saying `healthy` for about a second.  Writes made
    in that window look fine and never get a proof."""
    await asyncio.sleep(1.0)
    s2 = st.slave2
    flips = []
    for _ in range(8):
        t0 = time.time()
        await s2.set_mode('silent')
        t1 = await healthy(st, 'second', False, timeout=10)
        flips.append(None if t1 is None else round(t1 - t0, 3))
        await s2.set_mode('normal')
        await healthy(st, 'second', True, timeout=30)
        await asyncio.sleep(0.3)
    good = sorted(f for f in flips if f is not None)
    notes['silent_to_unhealthy_s'] = {'samples': flips, 'median': good[len(good) // 2] if good else None, 'min': good[0] if good else None,
                                      'max': good[-1] if good else None}
    ck('the health flag flipped in every silent episode (%d/8)' % len(good), len(good) == 8, flips)
    ck('the flip took about a second or more (>= 0.9 s): there IS a window', bool(good) and good[0] >= 0.9, good[:3])
    submitted, statuses = [], {}
    for burst in range(2):
        await s2.set_mode('silent')
        t_s = time.time()
        ps = []
        for k in range(15):
            wait = t_s + k * 0.1 - time.time()
            if wait > 0:
                await asyncio.sleep(wait)
            ps.append(st.standin.submit('silent_%02d' % (burst * 15 + k), 5.0 + 0.5 * (burst * 15 + k), ttl_ms=4000))   # every value distinct: a repeat would already be on the device
        submitted += ps
        await wait_until(lambda: all(st.event(p['action_id']) for p in ps), 30)
        await healthy(st, 'second', False, timeout=5)
        await asyncio.sleep(max(0.0, max(p['deadline_ms'] for p in ps) / 1000.0 + 1.5 - time.time()))
        await s2.set_mode('normal')
        await healthy(st, 'second', True, timeout=30)
        await asyncio.sleep(0.5)
    evs = [(p, st.event(p['action_id'])) for p in submitted]
    for _, ev in evs:
        key = str(short(ev))
        statuses[key] = statuses.get(key, 0) + 1
    notes['bridge_status_counts'] = statuses
    accepted = [(p, ev) for p, ev in evs if ev and ev['status'] == 'accepted']
    ck('%d requests were submitted in two bursts (>= 20)' % len(submitted), len(submitted) >= 20)
    ck('every request got an answer', all(ev is not None for _, ev in evs), statuses)
    ck('some writes fell into the window: accepted by the bridge while the link was already dead (%d)' % len(accepted), len(accepted) >= 2, statuses)
    ck('the accepted ones reported `undelivered_device: null` (the runtime still believed in the link)', all(ev.get('undelivered_device') is None for _, ev in accepted))
    verdicts = [verifier.derive(p, st.entry(p['target_key']), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys) for p, _ in accepted]
    notes['read_side_verdicts'] = {k: len([v for v in verdicts if (v['status'], v.get('reason')) == k]) for k in set((v['status'], v.get('reason')) for v in verdicts)}
    notes['read_side_verdicts'] = {str(k): v for k, v in notes['read_side_verdicts'].items()}
    ck('NOT ONE of them is `verified`', not any(v['status'] == 'verified' for v in verdicts), notes['read_side_verdicts'])
    ck('each ends `unknown` (no proof by the deadline)', all(v['status'] == 'unknown' for v in verdicts), notes['read_side_verdicts'])
    ck('no `verified` in the bridge trace', all(e.get('status') != 'verified' for e in st.trace()))


async def case_C6(st, ck, notes):
    """The target is an output the PROGRAM assigns on every scan: the write is overwritten and never reaches the device."""
    await asyncio.sleep(1.0)
    import check_registry
    findings = check_registry.check([PROG, FLOW], st.p('project'))
    notes['static_check'] = findings
    ck('the registry static check flags prog_setpoint as assigned by the program',
       any(f['key'] == 'prog_setpoint' and f['kind'] == 'assigned_by_program' for f in findings), findings)
    ck('...and does not flag flow_setpoint for that', not any(f['key'] == 'flow_setpoint' and f['kind'] == 'assigned_by_program' for f in findings), findings)
    t0 = time.time()
    p, ev = await submit_wait(st, 'prog_setpoint', 7.0, ttl_ms=5000)
    ck('the runtime accepted the write (200, undelivered null) -- it cannot know', ev is not None and ev['status'] == 'accepted', short(ev))
    await asyncio.sleep(1.0)
    sc, status = await st.rt('GET', '/status')
    row = vars_of(status).get('prog_sp')
    ck('the variable is back at the program value 1.5 (the write was overwritten)', row is not None and row['value'] == '1.5', row)
    fbrow = vars_of(status).get('prog_fb')
    ck('the feedback never moved: still 1.5', fbrow is not None and fbrow['value'] == '1.5', fbrow)
    ck('the device never received 7.0', not [e for e in st.slave_writes(130, t0) if e.get('value') == f32bits(7.0) >> 16], [hex(e.get('value') or 0) for e in st.slave_writes(130, t0)])
    await wait_deadline(p, 3.0)
    res = verifier.derive(p, st.entry('prog_setpoint'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('the read side ends `unknown` / no_proof_by_deadline, never verified', (res['status'], res.get('reason')) == ('unknown', 'no_proof_by_deadline'), res)


async def case_C8(st, ck, notes):
    """accepted, then the runtime is killed and restarted: IA2's state is cleared, the device kept the value, the new
    instance's feedback matches.  That must NOT verify the old action, and nothing may be resent."""
    await asyncio.sleep(1.0)
    first_id = ((await st.rt('GET', '/status'))[1] or {}).get('runtime_id')
    st.sampler.paused.set()                                                # no proof can be sampled before the kill
    await asyncio.sleep(0.4)
    p, ev = await submit_wait(st, 'flow_setpoint', 12.5, ttl_ms=6000)
    ck('accepted on the first runtime instance', ev is not None and ev['status'] == 'accepted' and ev.get('runtime_id') == first_id, short(ev))
    await asyncio.sleep(0.4)                                               # IA2 delivers 12.5 to the device
    ck('the device holds 12.5', st.slave.regs.get(100) == 0x4148, st.slave.regs.get(100))
    await st.restart_runtime()
    second_id = ((await st.rt('GET', '/status'))[1] or {}).get('runtime_id')
    ck('the runtime has a new instance id', second_id and second_id != first_id, (first_id, second_id))
    audit = await audit_list(st)
    ck('the new instance\'s /audit is empty (IA2 forgot the write)', audit == [], audit)
    boot = [e for e in st.slave_writes(100, 0.0) if e.get('words') == [0, 0]]
    notes['finding_boot_zero'] = ('the new runtime instance wrote zeros to the device outputs at start-up: after a restart the device does NOT keep '
                                  'the old value (observed: FC16@100 words [0, 0] right after the restart)')
    ck('observed: the restarted runtime zeroed the device output at boot', st.slave.regs.get(100) == 0 and len(boot) >= 2, st.slave.regs.get(100))
    st.sampler.paused.clear()
    await asyncio.sleep(1.5)
    res = verifier.derive(p, st.entry('flow_setpoint'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('natural case: the read side says unknown / runtime_restarted', (res['status'], res.get('reason')) == ('unknown', 'runtime_restarted'), res)
    # Plant the trap: the device shows 12.5 again (e.g. another writer, or a device that keeps its value across a restart).
    u = f32bits(12.5) & 0xFFFFFFFF
    st.slave.regs[100], st.slave.regs[101] = u >> 16, u & 0xFFFF
    await asyncio.sleep(1.5)
    sc, status = await st.rt('GET', '/status')
    ck('trap set: the new instance now reads 12.5 back, fresh', (vars_of(status).get('sp_flow_fb') or {}).get('value') == '12.5', vars_of(status).get('sp_flow_fb'))
    ck('there ARE matching fresh samples from the new instance (so the rule, not their absence, is what holds)',
       any(s['native_runtime_id'] == second_id and (next((v for v in s['variables'] if v['name'] == 'sp_flow_fb'), {}).get('decoded_value') == 12.5)
           for s in st.samples()))
    res = verifier.derive(p, st.entry('flow_setpoint'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('trap case: still unknown / runtime_restarted, not verified', (res['status'], res.get('reason')) == ('unknown', 'runtime_restarted'), res)
    ck('nothing was resent: still exactly one POST /write', len(posts(st)) == 1, wire_summary(st.wire()))
    await wait_deadline(p, 0.3)
    p2, ev2 = await submit_wait(st, 'flow_setpoint', 13.0, ttl_ms=20000)
    ck('a new request works on the new instance', ev2 is not None and ev2['status'] == 'accepted' and ev2.get('runtime_id') == second_id, short(ev2))
    res2 = await derive_until_settled(st, p2, 'flow_setpoint')
    ck('...and verifies', res2['status'] == 'verified', res2)


async def case_E1(st, ck, notes):
    """Feedback that is STALE (link dead) while its frozen value equals the target must not verify.  The samples that would
    show it fresh are withheld (a collector outage), so every sample the verifier sees is stale."""
    await asyncio.sleep(1.0)
    s2 = st.slave2
    u = f32bits(5.0) & 0xFFFFFFFF
    s2.regs[100], s2.regs[101] = u >> 16, u & 0xFFFF                          # the device already shows 5.0
    await asyncio.sleep(0.5)
    sc, status = await st.rt('GET', '/status')
    ck('precondition: the echo reads 5.0, fresh', (vars_of(status).get('d2_fb') or {}).get('value') == '5' and not ((vars_of(status).get('d2_fb') or {}).get('input') or {}).get('stale'))
    for attempt in range(3):
        st.sampler.paused.set()
        await s2.set_mode('silent')
        p, ev = await submit_wait(st, 'd2_set', 5.0, ttl_ms=6000)
        if ev is not None and ev['status'] == 'accepted':
            break
        notes.setdefault('retries', []).append(short(ev))
        st.sampler.paused.clear()
        await s2.set_mode('normal')
        await healthy(st, 'second', True, timeout=30)
        await asyncio.sleep(1.0)
    ck('the bridge accepted the write inside the silent window', ev is not None and ev['status'] == 'accepted', short(ev))
    await healthy(st, 'second', False, timeout=10)
    # Device health and input staleness can publish on different scans. Resume
    # only once the actual feedback prerequisite of this case is observable.
    stale_ready = False
    until = time.time() + 10
    while time.time() < until:
        _, status = await st.rt('GET', '/status')
        feedback = vars_of(status).get('d2_fb') or {}
        if (feedback.get('input') or {}).get('stale') is True:
            stale_ready = True
            break
        await asyncio.sleep(0.05)
    ck('precondition: feedback is explicitly stale before sampling resumes', stale_ready)
    st.sampler.paused.clear()
    await asyncio.sleep(2.0)
    stale_equal = [s for s in st.samples() if s['collected_at_ms'] > ev['sent_at_ms'] and
                   (next((v for v in s['variables'] if v['name'] == 'd2_fb'), {}).get('input') or {}).get('stale') is True and
                   next((v for v in s['variables'] if v['name'] == 'd2_fb'), {}).get('decoded_value') == 5.0]
    ck('samples exist that are stale AND equal to the target (the trap is really set)', len(stale_equal) >= 3, len(stale_equal))
    ck('...and none of the samples after the send is fresh', not [s for s in st.samples() if s['collected_at_ms'] > ev['sent_at_ms'] and
       (next((v for v in s['variables'] if v['name'] == 'd2_fb'), {}).get('input') or {}).get('stale') is False])
    await wait_deadline(p, 1.5)
    res = verifier.derive(p, st.entry('d2_set'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('not verified: unknown / feedback_stale', (res['status'], res.get('reason')) == ('unknown', 'feedback_stale'), res)
    await s2.set_mode('normal')


async def case_E2(st, ck, notes):
    """Observations from BEFORE the request that happen to equal the target must not count.  The sampler is paused from the
    moment of the request until after the proof window, so the only matching samples are pre-window ones (and later ones)."""
    await asyncio.sleep(1.0)
    s1 = st.slave
    u = f32bits(12.5) & 0xFFFFFFFF
    s1.regs[100], s1.regs[101] = u >> 16, u & 0xFFFF                          # the device already shows 12.5
    await asyncio.sleep(1.0)
    pre = [s for s in st.samples() if (next((v for v in s['variables'] if v['name'] == 'sp_flow_fb'), {}).get('decoded_value') == 12.5)]
    ck('precondition: fresh samples showing 12.5 exist BEFORE the request', len(pre) >= 3, len(pre))
    st.sampler.paused.set()
    p, ev = await submit_wait(st, 'flow_fast', 12.5, ttl_ms=2500)
    ck('accepted', ev is not None and ev['status'] == 'accepted', short(ev))
    entry = st.entry('flow_fast')
    await asyncio.sleep(max(0.0, ev['sent_at_ms'] / 1000.0 + 1.5 + 1.5 - time.time()))
    res = verifier.derive(p, entry, st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('with only pre-request samples the read side says unknown / no_proof_by_deadline', (res['status'], res.get('reason')) == ('unknown', 'no_proof_by_deadline'), res)
    st.sampler.paused.clear()
    await asyncio.sleep(1.5)
    post = [s for s in st.samples() if s['collected_at_ms'] > ev['sent_at_ms'] + 3000]
    ck('samples taken after the window also show 12.5 (and still do not help)', len(post) >= 3, len(post))
    res = verifier.derive(p, entry, st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('still unknown after those samples arrive', res['status'] == 'unknown', res)


async def case_E3(st, ck, notes):
    """No verify.feedback in the registry: the status can reach `accepted` and no further."""
    await asyncio.sleep(1.0)
    p, ev = await submit_wait(st, 'flow_nofb', 12.5, ttl_ms=3000)
    ck('accepted', ev is not None and ev['status'] == 'accepted', short(ev))
    await asyncio.sleep(1.5)
    sc, status = await st.rt('GET', '/status')
    ck('the feedback variable does show 12.5 (a proof exists in the world, but the registry names none)', (vars_of(status).get('sp_flow_fb') or {}).get('value') == '12.5')
    res = verifier.derive(p, st.entry('flow_nofb'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('before the deadline: accepted', res['status'] == 'accepted', res)
    await wait_deadline(p, 0.5)
    res = verifier.derive(p, st.entry('flow_nofb'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('after the deadline: unknown / no_feedback', (res['status'], res.get('reason')) == ('unknown', 'no_feedback'), res)
    ck('the claim says what was and was not established', 'not_claimed' in res and res['claim'] == '不知道（见原因）', res)


CASES = {
    'A2': (case_A2, [FLOW, BIG, LAMP]),
    'B2': (case_B2, [FLOW, LAMP]),
    'B4': (case_B4, [FLOW_WIDE], {'bridge_args': ['--governance-check', 'warn']}),
    'B6': (case_B6, [FLOW]),
    'B7': (case_B7, [FLOW]),
    'B8': (case_B8, [FLOW]),
    'B9': (case_B9, [FLOW]),
    'B11': (case_B11, [FLOW], {'stub': True, 'bridge_args': ['--governance-check', 'off']}),
    'C3': (case_C3, [D2]),
    'C4': (case_C4, SILENT),
    'C6': (case_C6, [PROG, FLOW]),
    'C8': (case_C8, [FLOW]),
    'E1': (case_E1, [D2]),
    'E2': (case_E2, [FLOW_FAST]),
    'E3': (case_E3, [FLOW_NOFB]),
}


# ======================================================================================================================
# Group 3
# ======================================================================================================================
def hook_env(st, point):
    return {'HOOK_AT': point, 'HOOK_DIR': st.dir}


async def wait_marker(st, point, timeout=20):
    path = os.path.join(st.dir, 'reached-' + point)
    return await wait_until(lambda: os.path.exists(path), timeout, 0.01)


def release(st, point):
    with open(os.path.join(st.dir, 'release-' + point), 'w') as f:
        f.write('go')


async def case_R1(st, ck, notes):
    """The bridge is killed (SIGKILL) after it journaled the intent and before it sent anything.  After the restart the action
    must stay `unknown` and must NOT be sent (D8 a)."""
    await asyncio.sleep(1.0)
    p = st.standin.payload('flow_setpoint', 12.5, ttl_ms=60000)
    line = st.standin.envelope(st.standin.payload_text(p))
    aid = p['action_id']
    st.standin.append_line(line)
    ck('the hook point (intent journaled, nothing sent) was reached', await wait_marker(st, 'after_intent'))
    kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
    ck('the journal holds the intent and nothing else', kinds == ['intent'], kinds)
    ck('no POST had been sent', len(posts(st)) == 0, wire_summary(st.wire()))
    await st.kill_bridge()
    ck('the bridge died by SIGKILL', st.bridge.returncode == -9, st.bridge.returncode)
    await st.start_bridge()                                               # a plain bridge: recovery runs at start-up
    ev = await wait_until(lambda: st.event(aid), 15)
    ck('after the restart the action is concluded as unknown / bridge_restarted', ev is not None and short(ev) == ('unknown', 'bridge_restarted'), short(ev))
    ck('...with a note that nothing was sent (D8)', ev is not None and 'not resent' in (ev.get('detail') or ''), ev and ev.get('detail'))
    st.standin.append_line(line)                                          # the same request arrives again
    await asyncio.sleep(2.5)
    ck('no POST /write was ever sent', len(posts(st)) == 0, wire_summary(st.wire()))
    ck('IA2 /audit is empty', await audit_list(st) == [])
    ck('the device never received the value', not [e for e in st.slave_writes(100) if e.get('value') == 0x4148])
    ck('one terminal record for the action; the replay added nothing', len([r for r in st.journal() if r['kind'] == 'terminal' and r.get('action_id') == aid]) == 1)
    note = await wait_until(lambda: next((e for e in st.trace() if e.get('kind') == 'reconcile_note' and e.get('action_id') == aid and e.get('final')), None), 15)
    ck('the periodic reconcile later recorded, finally: no /audit entry, and no claim that it was not executed',
       note is not None and note.get('audit_entry') == 'absent' and note.get('landed') is None, note)
    res = verifier.derive(p, st.entry('flow_setpoint'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    ck('the read side agrees: unknown', res['status'] == 'unknown', res)
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies across the kill', ok, n)


async def case_R2(st, ck, notes):
    """The bridge is killed after the write has LANDED in IA2.  (A) before it journaled the answer; (B) after it did.  Either way the
    restart must conclude the action from what IA2 itself recorded, and must not resend."""
    await asyncio.sleep(1.0)
    results = {}
    for name, key, value, point in (('A', 'flow_a', 12.5, 'after_http'), ('B', 'flow_b', 13.5, 'after_response')):
        await st.stop_bridge()
        await st.start_bridge(hooks=HOOKS_SYNC, env=hook_env(st, point))
        p = st.standin.payload(key, value, ttl_ms=30000)
        line = st.standin.envelope(st.standin.payload_text(p))
        aid = p['action_id']
        st.standin.append_line(line)
        ck('(%s) hook point %s reached' % (name, point), await wait_marker(st, point))
        kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
        ck('(%s) journal before the kill: %s' % (name, kinds), kinds == (['intent'] if name == 'A' else ['intent', 'response']), kinds)
        n_posts = len(posts(st))
        ck('(%s) the POST had been sent and answered' % name, n_posts == (1 if name == 'A' else 2), n_posts)
        await st.kill_bridge()
        await st.start_bridge()
        ev = await wait_until(lambda: st.event(aid), 15)
        ck('(%s) the restart concluded the action as accepted' % name, ev is not None and short(ev) == ('accepted', None), short(ev))
        ck('(%s) ...from the %s' % (name, '/audit entry' if name == 'A' else 'journal response record'),
           ev is not None and ('/audit entry' if name == 'A' else 'journal response record') in (ev.get('detail') or ''), ev and ev.get('detail'))
        ck('(%s) applied bits equal the request' % name, ev is not None and ev.get('applied_bits') == f32bits(value), ev and ev.get('applied_bits'))
        st.standin.append_line(line)
        await asyncio.sleep(2.0)
        ck('(%s) no POST after the restart (still %d)' % (name, n_posts), len(posts(st)) == n_posts, wire_summary(st.wire()))
        results[name] = (p, ev)
    audit = await audit_list(st)
    ck('IA2 /audit has exactly two entries, one per action', len(audit) == 2, len(audit))
    pa, eva = results['A']
    pb, evb = results['B']
    ck('(A) concluded from /audit alone: no sent_at_ms is claimed, but a scan-clock marker taken at recovery is (a sound lower bound)',
       'sent_at_ms' not in eva and isinstance(eva.get('post_scan_us'), int) and isinstance(eva.get('scan_anchor_ms'), int), (eva.get('sent_at_ms'), eva.get('post_scan_us')))
    ra = await derive_until_settled(st, pa, 'flow_a')
    ck('(A) the read side can verify it, but only with feedback observed after the recovery marker', ra['status'] == 'verified'
       and ra['observation_refs']['scan_timestamp_us'] > eva['post_scan_us'], ra)
    ck('(B) the journaled response keeps the send time', isinstance(evb.get('sent_at_ms'), int), evb.get('sent_at_ms'))
    rb = await derive_until_settled(st, pb, 'flow_b')
    ck('(B) and the read side verifies it', rb['status'] == 'verified', rb)
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies across both kills', ok, n)


# ---- R3: where the runtime is when the bridge's POST /write times out ------------------------------------------------------
# Ground truth the bridge cannot see comes from the fake device's write log, the VM value and /audit, plus the receive queue of
# the runtime's socket while it is stopped (netstat Recv-Q > 0: the request bytes are still unread).  The bridge's obligations
# are checks; what IA2 itself did is compared with R3_EXPECT and a difference is a FINDING.
R3_VALUE = 12.5
R3_HIGH_WORD = (f32bits(R3_VALUE) >> 16) & 0xFFFF             # 0x4148: the first register of the f32 on holding 100
R3_EXPECT = {                                                    # (executed on the VM/device, /audit has an entry); measured 2026-10-29
    'R3': (False, False),                                        # stopped before reading: the request is dropped with the connection
    'R3T1': (True, True),                                        # read and queued: applied after the client left, and audited
    'R3T1K': (True, True),                                       # control: the same, connection kept open -> audited
    'R3T1X': (True, True),                                       # read and queued, bridge SIGKILLed: applied and audited
    'R3T2': (True, True),                                        # applied and answered, answer lost: audited
}
# R3T1 and R3T1X expect the runtime fix (branch fix/control-write-detached).  A runtime built from main before it applies the
# write without an /audit entry, (True, False): a FINDING here, and `executed_unaudited` in the reconciliation (CONTRACT 11.5).


def netstat_runtime_side(local_port):
    """The runtime's end of the proxy's upstream connection in netstat (Proto Recv-Q Send-Q Local Foreign State): Recv-Q, Send-Q,
    state.  macOS prints `addr.port` (`netstat -an -p tcp`, the recorded runs); Linux `netstat -tan` prints `addr:port` (untested)."""
    cmd = ['netstat', '-an', '-p', 'tcp'] if sys.platform == 'darwin' else ['netstat', '-tan']
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=10).stdout
    ends = [('127.0.0.1%s%d' % (sep, harness.RT_PORT), '127.0.0.1%s%d' % (sep, local_port)) for sep in '.:']
    rows = []
    for line in out.splitlines():
        f = line.split()
        if len(f) >= 6 and (f[3], f[4]) in ends:
            rows.append({'recv_q': int(f[1]), 'send_q': int(f[2]), 'state': f[5], 'line': line.strip()})
    return rows


def write_conn(st):
    return next((r for r in st.wire_all() if r.get('ev') == 'req' and r.get('method') == 'POST' and r.get('path') == '/write'), None)


async def r3_submit(st, ck):
    p = st.standin.payload('flow_setpoint', R3_VALUE, ttl_ms=60000)
    st.standin.append_line(st.standin.envelope(st.standin.payload_text(p)))
    ck('hook point (intent journaled) reached', await wait_marker(st, 'after_intent'))
    return p


async def r3_timed_out(st, ck, aid):
    ev = await wait_until(lambda: st.event(aid), 30)
    ck('while the runtime is stopped the action concludes unknown / runtime_unreachable', ev is not None and short(ev) == ('unknown', 'runtime_unreachable'), short(ev))
    resp = next((r for r in st.journal() if r.get('action_id') == aid and r['kind'] == 'response'), None)
    ck('the journal records a transport timeout, not an answer', resp is not None and resp['http'].get('error_kind') == 'timeout', resp and resp['http'])
    ck('the post-check was skipped (the write got no answer)', any(r['kind'] == 'post_check' and r.get('skipped') for r in st.journal() if r.get('action_id') == aid))
    ck('exactly one POST was sent: no resend', len(posts(st)) == 1, wire_summary(st.wire()))
    return ev


async def r3_arm_stall(st, ck):
    """One 1 s scan stall (the watchdog needs 5 consecutive overruns), release the bridge, give the runtime 150 ms to read and queue."""
    s, body = await st.rt('POST', '/inject-scan-stall', {'stall_ms': 1000, 'scans': 1}, timeout=3.0)
    t_arm = time.time()
    ck('a single 1 s scan stall was armed', s == 200, (s, body))
    release(st, 'after_intent')
    req = await wait_until(lambda: write_conn(st), 5, 0.002)
    ck('the proxy forwarded the POST /write to the running runtime', req is not None)
    await asyncio.sleep(0.15)
    answered = [r for r in st.wire_all() if r.get('ev') == 'resp' and req and r.get('conn') == req['conn']]
    return req, t_arm, answered


async def r3_conclude(st, ck, notes, case_id, p, ev, after_t, reason='runtime_unreachable', from_audit=False):
    """`from_audit`: a restarted bridge found the /audit entry and concluded `accepted` from it (D8), so no reconcile note follows."""
    aid, label = p['action_id'], ev.get('label')
    note = None if from_audit else await wait_until(
        lambda: next((e for e in st.trace() if e.get('kind') == 'reconcile_note' and e.get('action_id') == aid and e.get('final')), None), 30)
    await asyncio.sleep(1.0)
    s, status = await st.rt('GET', '/status')
    row = st.var_row(status if isinstance(status, dict) else None, 'sp_flow')
    mine = [e for e in await audit_list(st) if e.get('origin') == label]
    dev = [e for e in st.slave_writes(100, after_t) if e.get('value') == R3_HIGH_WORD]
    vm = row is not None and row.get('bits') == f32bits(R3_VALUE)
    executed = bool(dev) or vm
    all_notes = [{k: e.get(k) for k in ('pass_no', 'landed', 'audit_entry', 'final', 'detail')}
                 for e in st.trace() if e.get('kind') == 'reconcile_note' and e.get('action_id') == aid]
    notes['truth'] = {'executed': executed, 'vm_bits': None if row is None else row.get('bits'), 'device_writes': [
        {k: e.get(k) for k in ('t_req', 'fc', 'addr', 'value')} for e in dev], 'audit_rows': mine}
    notes['reconcile_notes'] = all_notes
    if from_audit:
        ck('no reconcile note: the action was concluded from the /audit entry at the restart', all_notes == [], all_notes)
    else:
        ck('the bridge recorded a final reconcile note', note is not None, all_notes)
        want = ('found', True) if len(mine) == 1 else ('absent', None)
        ck('the final note states what /audit shows (%s, landed %s)' % want,
           note is not None and (note.get('audit_entry'), note.get('landed')) == want, (note, len(mine)))
    ck('no note claims the write was not executed (landed is never false)', all(n['landed'] is not False for n in all_notes), all_notes)
    ck('a note that claims landed is backed by what the device or VM shows', all(executed for n in all_notes if n['landed'] is True), notes['truth'])
    final = ('accepted', None) if from_audit else ('unknown', reason)
    ck('the status is still %s / %s (terminal states do not upgrade)' % final, short(st.event(aid)) == final, short(st.event(aid)))
    ck('still exactly one POST', len(posts(st)) == 1, wire_summary(st.wire()))
    res = verifier.derive(p, st.entry('flow_setpoint'), st.event_envelopes(), st.sample_envelopes(), now_ms(), event_keys=st.event_keys, sample_keys=st.sample_keys)
    if from_audit:
        ck('the read side verifies the accepted write against the feedback', res['status'] == 'verified', res)
    else:
        ck('the read side shows unknown with the reconcile note attached, never "not landed"',
           res['status'] == 'unknown' and 'reconcile' in res and res['reconcile']['landed'] is not False, res)
    seen = (executed, len(mine) == 1)
    notes['observed'] = {'executed': seen[0], 'audit_entry': seen[1]}
    if executed and not mine:
        notes['ia2_audit_gap'] = ('IA2 applied the write (device: %s, VM: %s) without an /audit entry: rt_write audits only after the queued '
                                  'write is acknowledged, and its handler is dropped when the client connection closes' % (bool(dev), vm))
    if seen != R3_EXPECT[case_id]:
        notes['finding'] = ('%s EXPECT (executed, audited) = %s; observed %s.  Update R3_EXPECT and the contract after checking why.'
                            % (case_id, R3_EXPECT[case_id], seen))


async def case_R3(st, ck, notes):
    """The runtime is SIGSTOPped before it reads the write request, for longer than the bridge's HTTP timeout, then continued."""
    await asyncio.sleep(1.0)
    p = await r3_submit(st, ck)
    st.signal_runtime(signal.SIGSTOP)
    t_stop = time.time()
    release(st, 'after_intent')
    req = await wait_until(lambda: write_conn(st), 10, 0.005)
    ck('the proxy forwarded the POST /write to the stopped runtime', req is not None)
    await asyncio.sleep(0.3)
    port = st.proxy.up_local_port.get(req['conn']) if req else None
    notes['netstat_while_stopped'] = q = netstat_runtime_side(port) if port else []
    ck('timing: the request bytes are still unread in the runtime socket (Recv-Q > 0)', bool(q) and q[0]['recv_q'] > 0, q)
    ev = await r3_timed_out(st, ck, p['action_id'])
    notes['stopped_for_s'] = round(time.time() - t_stop, 2)
    st.signal_runtime(signal.SIGCONT)
    await asyncio.sleep(3.0)
    await r3_conclude(st, ck, notes, 'R3', p, ev, req['t'] if req else t_stop)


async def r3_queued_then_stopped(st, ck, notes, case_id, keep_open):
    await asyncio.sleep(1.0)
    p = await r3_submit(st, ck)
    st.proxy.keep_write_upstream = keep_open
    req, t_arm, answered = await r3_arm_stall(st, ck)
    st.signal_runtime(signal.SIGSTOP)
    t_stop = time.time()
    notes['stop_after_stall_armed_s'] = round(t_stop - t_arm, 3)
    ck('timing: no answer before the stop (the write was still queued behind the stall)', not answered, answered)
    ck('timing: the stop came inside the 1 s stall', t_stop - t_arm < 0.8, t_stop - t_arm)
    port = st.proxy.up_local_port.get(req['conn']) if req else None
    await asyncio.sleep(0.2)
    notes['netstat_while_stopped'] = q = netstat_runtime_side(port) if port else []
    ck('timing: the runtime had read the whole request before the stop (Recv-Q = 0)', bool(q) and q[0]['recv_q'] == 0, q)
    ev = await r3_timed_out(st, ck, p['action_id'])
    await asyncio.sleep(0.3)
    notes['netstat_after_bridge_closed'] = qa = netstat_runtime_side(port) if port else []
    kept = [r for r in st.wire_all() if r.get('ev') == 'client_closed_upstream_kept_open']
    if keep_open:
        ck('control: the bridge gave up, the runtime connection stayed open (ESTABLISHED)', bool(kept) and bool(qa) and qa[0]['state'] == 'ESTABLISHED', (kept, qa))
    else:
        ck('the runtime connection closed with the bridge\'s (CLOSE_WAIT)', not kept and bool(qa) and qa[0]['state'] == 'CLOSE_WAIT', qa)
    notes['stopped_for_s'] = round(time.time() - t_stop, 2)
    st.signal_runtime(signal.SIGCONT)
    await asyncio.sleep(3.0)
    s, h = await st.rt('GET', '/health')
    notes['health_after'] = {k: h.get(k) for k in ('status', 'watchdog_tripped', 'latch_reason')} if isinstance(h, dict) else h
    ck('the injected stall did not trip the watchdog', isinstance(h, dict) and h.get('watchdog_tripped') is False, notes['health_after'])
    await r3_conclude(st, ck, notes, case_id, p, ev, req['t'] if req else t_stop)


async def case_R3T1(st, ck, notes):
    """The runtime has read the request and queued the write; a 1 s scan stall keeps it unapplied; SIGSTOP inside the stall."""
    await r3_queued_then_stopped(st, ck, notes, 'R3T1', False)


async def case_R3T1K(st, ck, notes):
    """Control for R3T1: identical, except the proxy keeps the runtime's connection open after the bridge gives up."""
    await r3_queued_then_stopped(st, ck, notes, 'R3T1K', True)


async def case_R3T1X(st, ck, notes):
    """The R3T1 window reached by a bridge crash: SIGKILL while the write is queued behind the stall, then a plain restart."""
    await asyncio.sleep(1.0)
    p = await r3_submit(st, ck)
    aid = p['action_id']
    req, t_arm, answered = await r3_arm_stall(st, ck)
    await st.kill_bridge()
    t_kill = time.time()
    notes['kill_after_stall_armed_s'] = round(t_kill - t_arm, 3)
    ck('timing: the bridge was killed before any answer (the write was still queued behind the stall)', not answered, answered)
    ck('timing: the kill came inside the 1 s stall', t_kill - t_arm < 0.8, t_kill - t_arm)
    kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
    ck('the journal holds the intent only', kinds == ['intent'], kinds)
    await asyncio.sleep(2.0)                                     # the stall ends; whatever the runtime does with the queued write is done
    await st.start_bridge()                                     # a plain bridge: recovery runs at start-up
    ev = await wait_until(lambda: st.event(aid), 15)
    # IA2 at main e6a38aa keeps no entry for this write, so recovery concludes unknown; a runtime that audits a write whose client went
    # away gives recovery the entry, and it concludes accepted from it (D8).  Either way R3_EXPECT decides whether this is a FINDING.
    from_audit = ev is not None and any(e.get('origin') == ev.get('label') for e in await audit_list(st))
    if from_audit:
        ck('after the restart the action is concluded accepted from the /audit entry', short(ev) == ('accepted', None), short(ev))
        ck('...and the detail says so', 'from the /audit entry' in (ev.get('detail') or ''), ev.get('detail'))
    else:
        ck('after the restart the action is concluded unknown / bridge_restarted', ev is not None and short(ev) == ('unknown', 'bridge_restarted'), short(ev))
        ck('...and the detail does not read the missing entry as "not executed"', ev is not None and 'does not show it was not executed' in (ev.get('detail') or ''),
           ev and ev.get('detail'))
    ck('exactly one POST in total: recovery did not resend', len(posts(st)) == 1, wire_summary(st.wire()))
    await r3_conclude(st, ck, notes, 'R3T1X', p, ev, req['t'] if req else t_kill, reason='bridge_restarted', from_audit=from_audit)


async def case_R3T2(st, ck, notes):
    """The runtime applied the write and answered; the proxy holds the answer and the runtime is SIGSTOPped at that moment."""
    await asyncio.sleep(1.0)
    p = await r3_submit(st, ck)
    held, gate = [], asyncio.Event()

    def on_held():
        st.signal_runtime(signal.SIGSTOP)
        held.append(time.time())
    st.proxy.on_held, st.proxy.hold_write = on_held, gate
    release(st, 'after_intent')
    req = await wait_until(lambda: write_conn(st), 5, 0.002)
    ck('the runtime answered and the proxy held the answer (runtime stopped at that moment)', bool(await wait_until(lambda: held, 5, 0.002)))
    hr = next((r for r in st.wire_all() if r.get('ev') == 'resp_held'), None)
    notes['held_answer'] = hr
    ck('timing: the held answer is a 200 (the write was applied before the stop)', hr is not None and hr.get('status') == 200, hr)
    ev = await r3_timed_out(st, ck, p['action_id'])
    notes['stopped_for_s'] = round(time.time() - held[0], 2) if held else None
    st.signal_runtime(signal.SIGCONT)
    gate.set()
    await asyncio.sleep(3.0)
    await r3_conclude(st, ck, notes, 'R3T2', p, ev, req['t'] if req else time.time())


FORBIDDEN_PARAMS = {'command', 'cmd', 'variable', 'ia2_variable', 'name', 'bits', 'value', 'pulse', 'pulse_ms', 'force', 'unforce',
                    'device', 'device_id', 'simulation', 'action_id_override'}


async def case_F1(st, ck, notes):
    """The control entry's tool surface (the stand-in toolset) and the unchanged read-only MCP."""
    import toolset
    await asyncio.sleep(1.0)
    ts = toolset.ControlToolset(st.standin, st.event_envelopes, st.sample_envelopes, st.targets, st.event_keys, st.sample_keys)
    surface = {n: list(inspect.signature(getattr(ts, n)).parameters) for n in dir(ts) if not n.startswith('_') and callable(getattr(ts, n))}
    notes['control_surface'] = surface
    ck('the control surface is exactly the two agreed tools with the agreed parameters', surface == toolset.ControlToolset.TOOLS, surface)
    ck('no parameter can carry an IA2 variable, a command, a bit pattern, a pulse or a force',
       not (set(p for ps in surface.values() for p in ps) & FORBIDDEN_PARAMS))
    for bad in ({'command': 'force'}, {'ia2_variable': 'sp_flow'}, {'pulse_ms': 100}, {'value': 1}):
        try:
            ts.ia2_set('flow_setpoint', 12.5, **bad)
            raised = False
        except TypeError:
            raised = True
        ck('ia2_set refuses an extra parameter %s' % list(bad), raised)
    ck('nothing was appended by the refused calls', open(st.p('requests.jsonl')).read() == '')
    r = ts.ia2_set('flow_setpoint', 12.5)
    line = json.loads(open(st.p('requests.jsonl')).read().strip().split('\n')[-1])
    payload = json.loads(line['payload'])
    ck('the request the tool produced has command ia2_set and exactly the contract fields',
       payload['command'] == 'ia2_set' and set(payload) == {'event_id', 'action_id', 'session_id', 'device_id', 'command', 'target_key', 'target',
                                                             'simulation', 'requested_at_ms', 'deadline_ms', 'requested_by'}, sorted(payload))
    end = time.time() + 15
    status = None
    while time.time() < end:
        status = ts.get_action_status(r['action_id'])
        if status['status'] != 'accepted' and status['status'] != 'requested':
            break
        await asyncio.sleep(0.2)
    ck('the tool reports the derived status (verified)', status['status'] == 'verified', status)
    # The archived read-only tool schema: verify its pinned surface, not a live installation.
    with open(READONLY_SCHEMA, encoding='utf-8') as f:
        schema = json.load(f)
    names = [t['name'] for t in schema['tools']]
    ck('the read-only MCP still has exactly its 7 tools', names == ['list_devices', 'query_observations', 'get_source_states', 'list_images',
                                                                     'read_image', 'query_action_results', 'describe_connections'], names)
    params = {t['name']: sorted(t['inputSchema'].get('properties', {})) for t in schema['tools']}
    notes['readonly_mcp_params'] = params
    write_shaped = {'command', 'cmd', 'value', 'bits', 'variable', 'ia2_variable', 'name', 'target', 'target_key', 'pulse', 'pulse_ms',
                    'force', 'unforce', 'write', 'set', 'setpoint'}
    ck('none of its tools has a write-shaped parameter (device_id etc. are selectors, not writes)', not (set(p for ps in params.values() for p in ps) & write_shaped), params)
    ck('the control tool names are disjoint from the read-only ones', not (set(surface) & set(names)))
    ck('the archived read-only tool schema has its recorded hash',
       sha256_file(READONLY_SCHEMA) == '07b66cb4329d3f8848ea3a2c90d3055d1f43c40aa1e9e2222e075b320d7c455f')
    notes['scope'] = 'F1 checks the stand-in surface and archived read-only schema; MCP and MCPPROD check the real control MCP.'


async def case_F2(st, ck, notes):
    """Forged and replayed requests, end to end."""
    await asyncio.sleep(1.0)
    sd = st.standin
    p = sd.payload('flow_setpoint', 12.5, ttl_ms=3000)
    text = sd.payload_text(p)
    aid = p['action_id']
    good = json.loads(sd.envelope(text))
    forged = {
        'no signature': json.dumps({k: v for k, v in good.items() if k != 'sig'}),
        'wrong signature': json.dumps(dict(good, sig='0' * 64)),
        'tampered target, original signature kept': json.dumps(dict(good, payload=text.replace('12.5', '39.5'))),
        'unknown key_id': sd.envelope(text, key_id='k9'),
        'signed with another secret': sd.envelope(text, secret=os.urandom(32)),
    }
    for line in forged.values():
        sd.append_line(line)
    await asyncio.sleep(1.5)
    unauth = [r for r in st.journal() if r['kind'] == 'unauthenticated']
    ck('five forged lines -> five `unauthenticated` journal records (one is a malformed envelope)', len(unauth) == 5 and
       sorted(r['reason'] for r in unauth) == ['bad_request', 'unauthenticated', 'unauthenticated', 'unauthenticated', 'unauthenticated'], [r['reason'] for r in unauth])
    ck('none of them mentions the action id (they never entered the dedup table)', all(aid not in json.dumps(r) for r in unauth))
    ck('no per-action trace event, nothing on the wire, nothing in IA2',
       [e for e in st.trace() if e.get('kind') in ('action_status', 'conflict_notice')] == [] and len(st.wire()) == 0 and await audit_list(st) == [])
    notice = await wait_until(lambda: next((e for e in st.trace() if e.get('kind') == 'bridge_notice'), None), 8)
    ck('...but an operator-visible bridge_notice reports the five lines (counts only, no content)',
       notice is not None and notice['counts']['unauthenticated'] == 5 and 'payload' not in json.dumps(notice), notice)
    # squatting: the genuine request with the SAME action_id must still run
    sd.append_line(sd.envelope(text))
    ev = await wait_until(lambda: st.event(aid), 10)
    ck('squatting failed: the genuine request with that action_id is accepted', ev is not None and ev['status'] == 'accepted', short(ev))
    # a validly signed payload that is not a request
    sd.submit_text('{"x":1}')
    # an old, never-delivered, validly signed request
    p_old = sd.payload('flow_setpoint', 14.0, ttl_ms=800)
    old_line = sd.envelope(sd.payload_text(p_old))
    await asyncio.sleep(1.3)
    sd.append_line(old_line)
    ev_old = await wait_until(lambda: st.event(p_old['action_id']), 10)
    ck('a signed request delivered after its deadline -> rejected / expired', ev_old is not None and short(ev_old) == ('rejected', 'expired'), short(ev_old))
    # replay inside the window
    sd.append_line(sd.envelope(text))
    await asyncio.sleep(1.0)
    ck('replay inside the window: nothing new', len(posts(st)) == 1 and len([e for e in st.trace() if e.get('action_id') == aid]) == 1)
    # replay after the deadline of an EXECUTED request
    await wait_deadline(p, 0.5)
    sd.append_line(sd.envelope(text))
    await asyncio.sleep(1.5)
    ck('replay after the deadline of an executed request: the recorded result, no second write',
       len(posts(st)) == 1 and short(st.event(aid)) == ('accepted', None) and len([e for e in st.trace() if e.get('action_id') == aid]) == 1)
    bad = [r for r in st.journal() if r['kind'] == 'gate_reject' and r.get('action_id') is None]
    ck('the validly signed non-request is a `bad_request`, journal only', any(r.get('reason') == 'bad_request' for r in bad), bad)
    audit = await audit_list(st)
    ck('IA2 /audit has exactly one entry in total', len(audit) == 1, len(audit))
    notes['deviation'] = ('TEST-CASES F2 says the forged requests are `rejected unauthenticated`; they are recorded in the JOURNAL only -- their '
                          'claimed action_id is not trusted, so no per-action trace event exists for them.  "No signature" is a malformed envelope '
                          '(reason bad_request), not an unauthenticated one.')


async def case_PROD(st, ck, notes):
    """Everything above runs the LAB flavour of the bridge.  This runs the PRODUCTION package (tools/package_prod.py): the same request,
    end to end, with no hook-loading code in it."""
    await asyncio.sleep(1.0)
    ck('the bridge running is the packaged one', st.bridge_run.endswith('prod-package/bridge_run.py'), st.bridge_run)
    p, ev = await submit_wait(st, 'flow_setpoint', 12.5, ttl_ms=20000)
    ck('accepted', ev is not None and short(ev) == ('accepted', None), short(ev))
    res = await derive_until_settled(st, p, 'flow_setpoint')
    ck('and verified by the read side', res['status'] == 'verified', res)
    start = next(r for r in st.journal() if r['kind'] == 'start')
    ck('the journal says it is a production build with no hooks', start.get('build') == 'prod' and start.get('hooks') is None and start.get('hooks_sha256') is None, start.get('build'))
    pkg_text = ' '.join(open(os.path.join(st.p('prod-package/bridge'), n), encoding='utf-8').read() for n in os.listdir(st.p('prod-package/bridge')) if n.endswith('.py'))
    ck('the package contains no trace of hook loading', not any(w in pkg_text for w in ('test-hooks', 'test_hooks', 'testhooks', 'importlib')))
    ck('exactly one POST /write', len(posts(st)) == 1, wire_summary(st.wire()))


CASES.update({
    'R1': (case_R1, [FLOW], {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_intent', 'HOOK_DIR': '{dir}'}}),
    'R2': (case_R2, [tgt('flow_a', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], verify=fb('sp_flow_fb')),
                     tgt('flow_b', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], verify=fb('sp_flow_fb'))]),
    'R3': (case_R3, [FLOW], {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_intent', 'HOOK_DIR': '{dir}'}}),
    'R3T1': (case_R3T1, [FLOW], {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_intent', 'HOOK_DIR': '{dir}'}}),
    'R3T1K': (case_R3T1K, [FLOW], {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_intent', 'HOOK_DIR': '{dir}'}}),
    'R3T1X': (case_R3T1X, [FLOW], {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_intent', 'HOOK_DIR': '{dir}'}}),
    'R3T2': (case_R3T2, [FLOW], {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_intent', 'HOOK_DIR': '{dir}'}}),
    'F1': (case_F1, [FLOW]),
    'F2': (case_F2, [FLOW]),
    'PROD': (case_PROD, [FLOW], {'prod': True}),
})
