"""What the journal keeps of an IA2 answer and of the check made right after the write, as facts a restart may decide on (P1: F08, F09).

The text of an answer is stored only to be read by people: `journal_view` cuts it at 4096 characters.  A restart must never rebuild a
decision from that copy -- the live answer may have failed to parse in full while its first 4096 characters happen to parse.  What it
may rely on is a versioned set of facts computed from the FULL answer and written in the SAME `response` record, bound to the action,
its label and the variable and bits of its intent.  A record from before that has no facts and is trusted only where nothing was lost.

The `post_check` record is the same kind of fact for the instant after the write (read failed, runtime changed, latched, scan-clock
marker).  A restart applies it with the rule the live path uses; what the runtime says now can only add limits, never remove one."""
import hashlib

from . import classify, strictjson
from .vocab import REASONS

FACTS_V = 1
POST_V = 1
STATES = ('accepted', 'modified', 'undelivered', 'rejected', 'unknown')
OUTCOME_KEYS = ('state', 'reason', 'detail', 'applied_bits', 'undelivered_device', 'also_modified', 'disable_target')


def untrusted(why):
    """A stored answer a restart cannot rely on: the same status the live path gives a body it cannot make sense of (M9)."""
    return {'state': 'unknown', 'reason': 'vm_error', 'detail': why}


def res_from_journal(http):
    """The stored view of an answer in the shape the client returns it, the body parsed again.  Only for records that say they are whole."""
    text = http.get('text') or ''
    try:
        parsed = strictjson.loads(text) if text else None
    except strictjson.StrictJSONError:
        parsed = None
    out = dict(http)
    out['json'] = parsed
    return out


# ---- the answer ------------------------------------------------------------------------------------------------------------------
def response_facts(aid, label, variable, bits, res, outcome):
    """Facts for the `response` record, from the answer as received and the outcome classified from it."""
    facts = {'v': FACTS_V, 'action_id': aid, 'label': label, 'variable': variable, 'bits': bits, 'transfer': res.get('kind'),
             'outcome': {k: outcome[k] for k in OUTCOME_KEYS if k in outcome}}
    text = res.get('text')
    if isinstance(text, str):
        facts['body_chars'] = len(text)
        facts['body_sha256'] = hashlib.sha256(text.encode('utf-8')).hexdigest()
    return facts


def facts_outcome(facts, aid, intent):
    """(outcome, None) from stored facts, or (None, why) when they cannot be used for this action."""
    if not isinstance(facts, dict):
        return None, 'the facts are not an object'
    if facts.get('v') != FACTS_V or type(facts.get('v')) is not int:
        return None, 'the facts have an unknown version (%r)' % (facts.get('v'),)
    for key, want in (('action_id', aid), ('label', intent.get('label')), ('variable', intent.get('variable')), ('bits', intent.get('bits'))):
        if type(facts.get(key)) is not type(want) or facts.get(key) != want:
            return None, 'the facts are not bound to this action (%s)' % key
    out = facts.get('outcome')
    if not isinstance(out, dict) or set(out) - set(OUTCOME_KEYS):
        return None, 'the stored outcome is not an object of known fields'
    state, reason = out.get('state'), out.get('reason')
    if state not in STATES:
        return None, 'the stored state is %r' % (state,)
    if reason is not None and reason not in REASONS:
        return None, 'the stored reason is %r' % (reason,)
    if state != 'accepted' and reason is None:
        return None, 'a %s outcome without a reason' % state
    if state in ('accepted', 'modified', 'undelivered') and type(out.get('applied_bits')) is not int:
        return None, 'a %s outcome without the applied bits' % state
    if state == 'undelivered' and not isinstance(out.get('undelivered_device'), str):
        return None, 'an undelivered outcome without the device'
    if any(k in out and type(out[k]) is not t for k, t in (('detail', str), ('also_modified', bool), ('disable_target', bool))):
        return None, 'a stored outcome field has the wrong type'
    return dict(out), None


def legacy_outcome(http, bits, variable):
    """The outcome for a `response` record written before facts existed.  A transport error needs no body.  A body is classified again only
    when the record SAYS it was kept whole (`text_truncated` is the boolean False); a cut copy, a missing marker or a marker of another
    type is not evidence of anything."""
    if not isinstance(http, dict):
        return untrusted('the response record holds no answer')
    if http.get('kind') == 'error' and isinstance(http.get('error_kind'), str):
        return classify.classify_write(http, bits, variable)
    if http.get('kind') == 'response' and type(http.get('status')) is int:
        if http.get('text_truncated') is False and isinstance(http.get('text'), str):
            return classify.classify_write(res_from_journal(http), bits, variable)
        return untrusted('only a cut or unmarked copy of the answer was stored; the full answer is not known, so it is not trusted')
    return untrusted('the stored answer has no usable shape')


# ---- the check made right after the write ------------------------------------------------------------------------------------------
def stored_post(records, label, runtime_id):
    """What a restart may take from the stored `post_check` records of an action:
    ('none', None) when none was recorded; ('ok', post) for exactly one complete, matching record; ('unusable', why) otherwise.
    `post` has the shape the live path decides on.  A record without `v` is a P0-era one: usable when complete."""
    if not records:
        return 'none', None
    if len(records) > 1:
        return 'unusable', 'more than one post_check record'
    rec = records[0]
    if 'v' in rec and (rec['v'] != POST_V or type(rec['v']) is not int):
        return 'unusable', 'a post_check record of an unknown version (%r)' % (rec['v'],)
    if 'v' in rec and (rec.get('label') != label or rec.get('runtime_id') != runtime_id):
        return 'unusable', 'the post_check record names another label or runtime'
    if 'skipped' in rec:
        return 'unusable', 'no check was made (%s)' % rec['skipped']
    if type(rec.get('read_failed')) is not bool:
        return 'unusable', 'read_failed is missing or not a boolean'
    if rec['read_failed']:
        return 'ok', {'read_failed': True, 'detail': rec.get('detail')}
    scan = rec.get('scan')
    if type(rec.get('identity_changed')) is not bool or type(rec.get('latched_after')) is not bool:
        return 'unusable', 'identity_changed / latched_after are missing or not booleans'
    if not isinstance(scan, dict) or type(scan.get('ts_us')) is not int or type(scan.get('anchor_ms')) is not int:
        return 'unusable', 'the scan-clock marker is missing or malformed'
    return 'ok', {'read_failed': False, 'identity_changed': rec['identity_changed'], 'latched_after': rec['latched_after'], 'scan': scan}
