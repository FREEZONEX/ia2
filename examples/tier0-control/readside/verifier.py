"""Read-side verifier, v2 (D4): derives an action's status from the bridge's trace and the sampler's observations.

It imports nothing from the bridge.  It starts from `accepted` plus independent observations and ignores every
`verified` claim anyone wrote (v1's derive_action started from a `verified` event).

What it trusts and what it does not:
  * The target value, deadline and time marks come from the BRIDGE'S EVENT, never from the request record (which lives on an
    unauthenticated topic).  The first argument is used only to say which action is meant.
  * The first bridge event for an action wins (terminal states do not upgrade); a later contradictory one is ignored.
  * Ordering of feedback against the write is done in the RUNTIME'S OWN SCAN CLOCK: the bridge records `post_scan_us` (the scan
    clock in the first /status read after the write was answered) and `scan_anchor_ms` (its own wall clock minus that scan
    time); a sample counts only if its `scan_timestamp_us` is later than `post_scan_us` and no later than min(post_scan_us +
    settle_ms, the deadline mapped through the anchor).  The sampler's wall-clock estimate is never used, so a sampler running
    on another host with a skewed clock cannot make an old observation look new.

CONTRACT section 7:
  - the feedback is the registry's verify.feedback variable, an Input-mapped one (it carries input.stale == false);
  - its device is healthy in the same sample; same runtime instance as the bridge recorded;
  - |value - requested| <= tolerance (compared with the REQUEST, not with a clamped value), as an EXACT absolute difference chosen by
    the registry's type for the target (CONTRACT 7.8): BOOL equal after normalising to 0/1 (the tolerance plays no part); INT, DINT
    and REAL as rational numbers, integers never through float, a float read as the shortest decimal text that round-trips (so
    12.55 is on the bound of 12.5 +- 0.05 and the f32 value 12.550000190734863 is outside it).  A value that is not a finite number
    of the right kind (NaN, +-inf, text, a bool for a number, a number for a BOOL) is no observation at all, and a request or
    tolerance that is not a usable number proves nothing;
  - same sample: no watchdog trip, no fault, no latch, target not forced; not from the future of the evaluation moment."""
import json
import math
import os
from fractions import Fraction

with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'contract_vocab.json'), encoding='utf-8') as _f:
    VOCAB = json.load(_f)
BRIDGE_STATES = frozenset(['accepted'] + VOCAB['bridge_terminal'])
FUTURE_SLACK_US = 500 * 1000
UNKNOWN_GRACE_US = 1000 * 1000
NUMBER_TYPES = ('INT', 'DINT', 'REAL')


def _exact(x):
    """A finite int or float as an exact rational; None for anything else (a bool, text, None, NaN, +-inf, a container).

    An int never passes through float.  A float is taken as the shortest decimal text that reads back as the same float -- the number
    the JSON that carried it says -- so 12.55 is 1255/100.  Fractions have no precision context: no step rounds."""
    if type(x) is int:
        return Fraction(x)
    if type(x) is float and math.isfinite(x):
        return Fraction(repr(x))
    return None


def _requested_number(kind, target):
    """(exact number, None) or (None, why not): the request in the bridge's event, read by the registry's type for the target."""
    if kind == 'BOOL':
        if type(target) is bool or (type(target) is int and target in (0, 1)) or (type(target) is float and target in (0.0, 1.0)):
            return Fraction(int(target)), None
        return None, 'the requested value is not a BOOL'
    if kind in NUMBER_TYPES:
        want = _exact(target)
        if want is None:
            return None, 'the requested value is not a finite number'
        if kind != 'REAL' and want.denominator != 1:
            return None, 'the requested value is not an integer, but the registry type is %s' % kind
        return want, None
    return None, 'the registry entry has no usable type (%r)' % (kind,)


def _feedback_number(kind, value):
    """The sampled value as an exact number, or None when it cannot be an observation of a variable of this type."""
    if kind == 'BOOL':
        return Fraction(int(value)) if type(value) is bool or (type(value) is int and value in (0, 1)) else None
    return _exact(value)


def _result(status, aid, reason=None, **extra):
    out = {'status': status, 'action_id': aid, 'reason': reason,
           'claim': VOCAB['claims'][status]['claim'], 'not_claimed': VOCAB['claims'][status]['not_claimed']}
    out.update(extra)
    return out


def _forced(forces, variable):
    short = variable.split('.')[-1].lower()
    for item in forces or []:
        name = (item.get('name') or item.get('variable')) if isinstance(item, dict) else item
        if not isinstance(name, str) or name.split('.')[-1].lower() == short:
            return True
    return False


def _derive_authenticated(request, entry, events, samples, as_of_ms, registry_sha256=None):
    """`request` may be the request dict or just the action id: only the id is used."""
    aid = request['action_id'] if isinstance(request, dict) else request
    related = [e for e in events if e.get('kind') == 'action_status' and e.get('action_id') == aid and e.get('status') in BRIDGE_STATES]
    if not related:
        return _result('requested', aid)
    ev = sorted(related, key=lambda e: e.get('observed_at_ms', 0))[0]          # the first word counts
    if ev['status'] != 'accepted':
        extra = {}
        notes = sorted([e for e in events if e.get('kind') == 'reconcile_note' and e.get('action_id') == aid], key=lambda e: e.get('pass_no', 0))
        if notes:
            last = notes[-1]
            # Bridges before v0.9 wrote `landed: false` for an absent entry; that never showed the write was not executed.
            seen = last.get('audit_entry')
            if seen not in ('found', 'absent', 'unavailable'):
                seen = 'found' if last.get('landed') is True else 'absent' if last.get('landed') is False else 'unavailable'
            extra['reconcile'] = {'audit_entry': seen, 'landed': True if seen == 'found' else None, 'final': last.get('final'),
                                  'detail': last.get('detail')}
        return _result(ev['status'], aid, ev.get('reason'), applied_value=ev.get('applied_value'), **extra)
    if registry_sha256 is not None and ev.get('registry_sha256') != registry_sha256:
        return _result('unknown', aid, 'no_proof_by_deadline', detail='the bridge used a different registry than the one the read side holds')
    verify = entry.get('verify')
    deadline_ms = ev.get('deadline_ms')
    if verify is None:
        return _result('unknown', aid, 'no_feedback') if isinstance(deadline_ms, int) and as_of_ms > deadline_ms else _result('accepted', aid)
    lo_us, anchor_ms, target = ev.get('post_scan_us'), ev.get('scan_anchor_ms'), ev.get('requested_value')
    if not (type(lo_us) is int and type(anchor_ms) is int and type(deadline_ms) is int):
        # A legacy/incomplete event without a write or recovery marker cannot order feedback after the action.
        return _result('unknown', aid, 'no_proof_by_deadline', detail='the accepted event has no scan-clock marker of the write')
    kind = entry.get('type')
    want, why_not = _requested_number(kind, target)
    tol = Fraction(0) if kind == 'BOOL' else _exact(verify.get('tolerance'))
    if want is None or tol is None or tol < 0:
        # Nothing the feedback says can match a request or a tolerance that is not a usable number: decide now, do not wait for the deadline.
        return _result('unknown', aid, 'no_proof_by_deadline', detail=why_not or 'the registry tolerance is not a finite number >= 0')
    hi_us = min(lo_us + verify['settle_ms'] * 1000, (deadline_ms - anchor_ms) * 1000)
    as_of_us = (as_of_ms - anchor_ms) * 1000
    saw_untrusted_feedback = False
    unusable = 0
    for s in sorted(samples, key=lambda x: x.get('scan_timestamp_us', 0)):
        t = s.get('scan_timestamp_us')
        if s.get('native_runtime_id') != ev.get('runtime_id') or type(t) is not int:
            continue
        if not lo_us < t <= hi_us or t > as_of_us + FUTURE_SLACK_US:
            continue
        var = next((v for v in s.get('variables', []) if v.get('name') == verify['feedback']), None)
        if var is None:
            continue
        inp = var.get('input')
        healthy = isinstance(inp, dict) and any(d.get('name') == inp.get('device') and d.get('healthy') is True
                                                for d in s.get('device_health') or [])
        if not isinstance(inp, dict) or inp.get('stale') is not False or not healthy:
            saw_untrusted_feedback = True
            continue
        if s.get('watchdog_tripped') is not False or s.get('fault') is not None or s.get('latch_reason') \
                or _forced(s.get('forces'), entry['ia2_variable']):
            continue
        value = var.get('decoded_value')
        got = _feedback_number(kind, value)
        if got is None:
            unusable += 1             # not a number this variable can hold (NaN, +-inf, text, wrong kind): never repaired into a proof
            continue
        # Inclusive, exact, and the same absolute tolerance at every magnitude.  BOOL: the two must be equal.
        if (got == want) if kind == 'BOOL' else (abs(got - want) <= tol):
            return _result('verified', aid, observation_refs={
                'sample_event_id': s['event_id'], 'scan_count': s['scan_count'], 'scan_timestamp_us': t,
                'observed_at_ms': s.get('observed_at_ms'), 'variable': verify['feedback'], 'value': value, 'device': inp.get('device')})
    # A different runtime instance has appeared since the request was sent: its matching feedback proves nothing about this
    # action (a restart clears IA2's state), so say so instead of waiting.
    sent = ev.get('sent_at_ms')
    if isinstance(sent, int) and any(s.get('native_runtime_id') != ev.get('runtime_id') and isinstance(s.get('collected_at_ms'), int)
                                     and s['collected_at_ms'] > sent for s in samples):
        return _result('unknown', aid, 'runtime_restarted')
    if as_of_us > hi_us + UNKNOWN_GRACE_US:
        extra = {'detail': '%d sample(s) in the proof window carried a feedback value that cannot be an observation of a %s variable '
                           '(not a finite number of the right kind)' % (unusable, kind)} if unusable else {}
        return _result('unknown', aid, 'feedback_stale' if saw_untrusted_feedback else 'no_proof_by_deadline', **extra)
    return _result('accepted', aid)


def derive(request, entry, events, samples, as_of_ms, registry_sha256=None, *, event_keys, sample_keys, revoked=()):
    """Authenticate both sources before evaluating evidence. There is no unsigned mode."""
    from .auth import separate_keys, verified_records
    separate_keys(event_keys, sample_keys)
    accepted_events, bad_events = verified_records(events, event_keys, 'event', revoked)
    accepted_samples, bad_samples = verified_records(samples, sample_keys, 'sample', revoked)
    try:
        result = _derive_authenticated(request, entry, accepted_events, accepted_samples, as_of_ms, registry_sha256)
    except (ValueError, TypeError, KeyError, AttributeError, OverflowError):
        aid = request.get('action_id') if isinstance(request, dict) else request
        result = _result('unknown', aid, 'no_proof_by_deadline', detail='malformed authenticated evidence')
    result['rejected_envelopes'] = bad_events + bad_samples
    result['rejected_by_role'] = {'event': bad_events, 'sample': bad_samples}
    return result
