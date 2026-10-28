"""P1 end-to-end cases.  Each registers in CASES like the P0 cases (fn, registry targets[, options]).

  P1F5   F05  a feedback of +inf / -inf coming off the wire as a real REAL bit pattern never proves anything
  P1F6   F06  a full-width DINT feedback one count off the request never proves anything (tolerance 0), an equal one still does

Both go through the whole chain: fake Modbus device -> sandbox ia2-runtime -> Input-mapped feedback variable -> sampler (which
decodes the bit pattern) -> read-side verifier, with one POST per action.  The device is made to keep a value in its echo register
by FrozenRegs: the write is acknowledged on the wire, the register does not change."""
import asyncio
import math
import time

import verifier
from cases_p0 import fb, f32bits, posts, short, submit_wait, tgt
from harness import now_ms, verify_chain, wait_until, wire_summary

FLOW = tgt('flow_setpoint', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], verify=fb('sp_flow_fb'))
BIG32 = tgt('big32_setpoint', 'big', 'DINT', -2147483648, 2147483647, require_devices=['coupler'], verify=fb('big_fb', 0.0))
FULL_WIDTH_DINT = (-2147483648.0, 2147483647.0)


class FrozenRegs(dict):
    """The fake slave's register table, except that some registers keep a value whatever is written to them (a stuck echo channel)."""

    def __init__(self, regs):
        super().__init__(regs)
        self.frozen = {}

    def freeze(self, values):
        self.frozen = dict(values)
        dict.update(self, self.frozen)

    def thaw(self):
        self.frozen = {}

    def __setitem__(self, addr, value):
        if addr not in self.frozen:
            dict.__setitem__(self, addr, value)


def feedback(st, name):
    """[(scan_timestamp_us, decoded_value)] of one variable over every sample the sampler has written so far."""
    return [(s['scan_timestamp_us'], v.get('decoded_value')) for s in st.samples() for v in s['variables'] if v.get('name') == name]


def action_posts(st, p):
    label = 'tier0.lab.%s' % p['action_id'][:8]
    return [r for r in posts(st) if r['origin'] == label]


async def settle(st, p, key, timeout=14):
    """Ask the verifier until it stops saying `accepted`.  Returns (the last answer, every status it gave on the way)."""
    seen, res = [], None
    end = time.time() + timeout
    while time.time() < end:
        res = verifier.derive(p, st.entry(key), st.trace(), st.samples(), now_ms())
        seen.append(res['status'])
        if res['status'] != 'accepted':
            break
        await asyncio.sleep(0.2)
    return res, seen


async def clear_window(ev, settle_ms=5000):
    """The bridge keeps a target busy for verify.settle_ms after an accepted write whatever the read side concluded (G6 single flight)."""
    await asyncio.sleep(max(0.0, ev['sent_at_ms'] / 1000.0 + settle_ms / 1000.0 + 0.5 - time.time()))


def tail_is(values, predicate, n=3):
    tail = [v for _, v in values[-n:]]
    return len(tail) == n and all(predicate(v) for v in tail)


def words_of(bits32):
    """The two 16-bit registers (hi_lo) of a 32-bit pattern."""
    return [(bits32 >> 16) & 0xFFFF, bits32 & 0xFFFF]


# IA2 hands an output to the device only when its value CHANGES, so every action in a case asks for a value of its own.


# ====================================================================================================================
async def case_P1F5(st, ck, notes):
    await asyncio.sleep(1.0)
    regs = FrozenRegs(st.slave.regs)
    st.slave.regs = regs
    runs = []
    for name, words, positive, request in (('+inf', (0x7F80, 0x0000), True, 12.5), ('-inf', (0xFF80, 0x0000), False, 13.5)):
        regs.freeze({100: words[0], 101: words[1]})
        seen_on_the_wire = await wait_until(lambda: tail_is(feedback(st, 'sp_flow_fb'), lambda v: isinstance(v, float) and math.isinf(v) and (v > 0) == positive), 10)
        ck('%s: the sampler decoded the device\'s bit pattern as %s before the write' % (name, name), seen_on_the_wire, feedback(st, 'sp_flow_fb')[-3:])
        sc, status = await st.rt('GET', '/status')
        row = st.var_row(status, 'sp_flow_fb')
        notes['runtime_row_%s' % name] = row
        p, ev = await submit_wait(st, 'flow_setpoint', request, ttl_ms=20000)
        ck('%s: the bridge accepted a normal, finite request (%s)' % (name, request),
           short(ev) == ('accepted', None) and ev.get('applied_bits') == f32bits(request), short(ev))
        res, seen = await settle(st, p, 'flow_setpoint')
        runs.append((name, res, seen))
        ck('%s: the verifier never said `verified`' % name, 'verified' not in seen, sorted(set(seen)))
        ck('%s: after the window it says unknown / no_proof_by_deadline, with a diagnosis' % name,
           (res['status'], res.get('reason')) == ('unknown', 'no_proof_by_deadline') and bool(res.get('detail')), res)
        after = [v for t, v in feedback(st, 'sp_flow_fb') if ev.get('post_scan_us') is not None and t > ev['post_scan_us']]
        ck('%s: samples taken after the write all carry the non-finite value (the bad observation was really in the window)' % name,
           len(after) >= 3 and all(isinstance(v, float) and math.isinf(v) for v in after), after[:5])
        mine = action_posts(st, p)
        ck('%s: exactly one POST /write for this action' % name, len(mine) == 1, len(mine))
        writes = [e for e in st.slave_writes(100, p['requested_at_ms'] / 1000.0 - 1) if e.get('words') == words_of(f32bits(request) & 0xFFFFFFFF)]
        ck('%s: the device acknowledged the finite write once' % name, len(writes) == 1, len(writes))
        notes['result_%s' % name] = res
        await clear_window(ev)
    # bad samples first, then the device reports the requested value: the good sample proves it, the bad ones are skipped
    regs.freeze({100: 0x7F80, 101: 0x0000})
    await wait_until(lambda: tail_is(feedback(st, 'sp_flow_fb'), lambda v: isinstance(v, float) and math.isinf(v)), 10)
    p, ev = await submit_wait(st, 'flow_setpoint', 14.5, ttl_ms=20000)
    ck('bad then good: accepted', short(ev) == ('accepted', None), short(ev))
    await asyncio.sleep(1.0)
    regs.thaw()
    regs[100], regs[101] = words_of(f32bits(14.5) & 0xFFFFFFFF)         # the device now reports what was asked for
    res, seen = await settle(st, p, 'flow_setpoint')
    refs = res.get('observation_refs') or {}
    ck('bad then good: the verifier says verified', res['status'] == 'verified', res)
    ck('bad then good: the proof is a finite sample of the feedback variable that carries 14.5', refs.get('variable') == 'sp_flow_fb' and refs.get('value') == 14.5, refs)
    skipped = [v for t, v in feedback(st, 'sp_flow_fb') if ev.get('post_scan_us') is not None and ev['post_scan_us'] < t < (refs.get('scan_timestamp_us') or 0)]
    ck('bad then good: non-finite samples really came first inside the window', any(isinstance(v, float) and math.isinf(v) for v in skipped), skipped[:6])
    ck('bad then good: exactly one POST /write for this action', len(action_posts(st, p)) == 1, len(action_posts(st, p)))
    notes['result_bad_then_good'] = res
    w = wire_summary(st.wire())
    ck('three actions, three POSTs in all, nothing outside the allowed routes', len(w['posts']) == 3 and not w['outside_allowed'], w)
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies', ok, n)


async def case_P1F6(st, ck, notes):
    await asyncio.sleep(1.0)
    regs = FrozenRegs(st.slave.regs)
    st.slave.regs = regs
    # (request, the value the device's echo is stuck at -- None: the device is free and echoes what it is given, verified?)
    plan = [(2000000000, 2000000001, False), (2000000002, None, True), (2000000004, 2000000003, False)]
    for request, stuck, expect_verified in plan:
        name = 'request %d, feedback %s' % (request, 'equal' if stuck is None else stuck)
        if stuck is not None:
            regs.freeze({120: words_of(stuck)[0], 121: words_of(stuck)[1]})
            on_wire = await wait_until(lambda: tail_is(feedback(st, 'big_fb'), lambda v: v == stuck), 10)
            ck('%s: the runtime reports the stuck value before the write' % name, on_wire, feedback(st, 'big_fb')[-3:])
        else:
            regs.thaw()
        t0 = time.time()
        p, ev = await submit_wait(st, 'big32_setpoint', request, ttl_ms=20000)
        ck('%s: accepted, so the verifier is what decides (not the bridge\'s range gate)' % name,
           short(ev) == ('accepted', None) and ev.get('applied_bits') == request, short(ev))
        res, seen = await settle(st, p, 'big32_setpoint')
        if expect_verified:
            ck('%s: verified' % name, res['status'] == 'verified', res)
            ck('%s: by the DINT feedback variable' % name, (res.get('observation_refs') or {}).get('value') == request, res.get('observation_refs'))
        else:
            ck('%s: never verified (tolerance 0)' % name, 'verified' not in seen, sorted(set(seen)))
            ck('%s: after the window unknown / no_proof_by_deadline' % name, (res['status'], res.get('reason')) == ('unknown', 'no_proof_by_deadline'), res)
        mine = action_posts(st, p)
        ck('%s: exactly one POST /write for this action' % name, len(mine) == 1, len(mine))
        writes = [e for e in st.slave_writes(120, t0 - 1) if e.get('words') == words_of(request)]
        ck('%s: the device acknowledged the write once' % name, len(writes) == 1, len(writes))
        notes['result: ' + name] = res
        regs.thaw()
        await clear_window(ev)
    w = wire_summary(st.wire())
    ck('three actions, three POSTs in all, nothing outside the allowed routes', len(w['posts']) == 3 and not w['outside_allowed'], w)
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies', ok, n)


CASES = {
    'P1F5': (case_P1F5, [FLOW]),
    'P1F6': (case_P1F6, [BIG32], {'project': {'big_range': FULL_WIDTH_DINT}}),
}
