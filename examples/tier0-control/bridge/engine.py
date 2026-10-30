"""The request engine: gates, intent, send, post-check, classify, record, and crash recovery.  CONTRACT sections 5-8,
DESIGN-1A 3.5.

Implementation notes that differ from or sharpen the contract text (see RESULT-SLICE.md, RESULT-P0.md, RESULT-FIXES.md):
  * G5 (dedup) runs right after the signature and shape checks, before G2-G4.  A replay of an executed action must
    return the recorded result, not a fresh `expired`, or a replay could overwrite the status the Agent already saw.
  * "Open" for single-flight (G6): sent but not concluded, or `accepted` and still inside min(deadline, single_flight_ms).
  * The clock-skew limit of G4 is judged against when the line ARRIVED, so time spent queued behind a slow runtime is not
    mistaken for staleness.
  * A request with the same action_id but different bytes gets a `conflict_notice` event; it never touches the status
    of the original action.  Unauthenticated lines are journaled only (their claimed action_id is not trusted).
  * Crash recovery never resends (D8).  It concludes an in-flight action from what IA2 recorded (the journal's response
    record, else /audit by label). An action concluded from /audit alone has no send timestamp, but can carry a scan-clock
    marker taken at recovery on the same runtime instance; only feedback after that marker can verify it.
  * Writes that ended `unknown` for a transport/VM reason get `reconcile_note`s (two looks, the second after a pause).  `audit_entry`
    says what /audit showed: found / absent / unavailable.  Only `found` is a claim (`landed: true`); otherwise `landed` is null.  An
    absent entry never means "not executed" (R3T1/R3T1X): the runtime audits a write after the scan loop applied it, and records nothing
    if the HTTP connection closed while the write was queued.  The status itself never changes: terminal states do not upgrade.
  * `accepted` events carry the runtime's own scan clock at the first post-write /status read (`post_scan_us`) and the offset between
    the bridge's wall clock and that scan clock (`scan_anchor_ms`): the read side orders feedback against the write in the
    runtime's own clock, never against another host's wall clock.
  * What a restart may rely on is written with the record, not rebuilt from the diagnostic copy of the text (P1, F08/F09): the `response`
    record carries versioned `facts` computed from the full answer, the `post_check` record is applied again with the live rule, and what
    the runtime says after the restart can only add limits (bridge/facts.py).
  * Request-processing exceptions quarantine the line. Publishing an already durable result is separate: failures stay pending
    for replay/startup/housekeeping, without reclassifying the result or quarantining the request.
  * No retry, no retraction, no resend, ever (D1, D8)."""
import hashlib
from itertools import islice
import json
import math
import os
import re
import time

from . import auth, classify, facts, gates, northbound_guard, strictjson, values, runtime_schema
from .errors import Reject
from .ia2client import journal_view
from .journal import JournalWriteError
from .vocab import CLAIMS

GLOBAL_PER_MIN = 60
RECONCILE_AFTER_MS = 1000
NOTICE_EVERY_MS = 60000


class NoHooks:
    pass


def now_ms():
    return int(time.time() * 1000)


def sha256_text(text):
    return hashlib.sha256(text.encode('utf-8')).hexdigest()


def make_label(payload):
    agent = payload['requested_by'].split(':', 1)[-1]
    agent = re.sub(r'[^A-Za-z0-9._-]', '_', agent)[:32] or 'agent'
    return 'tier0.%s.%s' % (agent, payload['action_id'][:8])


def find_audit_row(audit, label, variable, bits):
    if not isinstance(audit, list):
        return None
    rows = [e for e in audit if isinstance(e, dict) and e.get('origin') == label
            and e.get('name') == variable and e.get('requested') == bits]
    return rows[-1] if rows else None


def outcome_from_audit_row(row, bits):
    """What IA2 itself recorded for a write, as a status.  Mirrors classify.classify_write for the cases /audit can show."""
    result, applied = row.get('result'), row.get('applied')
    if not isinstance(result, str):
        return {'state': 'unknown', 'reason': 'vm_error', 'detail': 'audit entry has no result text'}
    if 'rejected by project governance' in result:
        return {'state': 'rejected', 'reason': 'governance_denied'}
    if result.startswith('unknown variable'):
        return {'state': 'rejected', 'reason': 'unknown_variable', 'disable_target': True}
    if type(applied) is not int:
        return {'state': 'unknown', 'reason': 'vm_error', 'detail': 'audit entry has no applied value'}
    m = re.match(r"ok; not delivered to device '([^']*)'", result)
    if m:
        return {'state': 'undelivered', 'reason': 'link_down', 'applied_bits': applied, 'undelivered_device': m.group(1),
                'also_modified': applied != bits}
    if result == 'clamped' or (result == 'ok' and applied != bits):
        return {'state': 'modified', 'reason': 'clamped', 'applied_bits': applied}
    if result == 'ok':
        return {'state': 'accepted', 'reason': None, 'applied_bits': applied}
    return {'state': 'unknown', 'reason': 'vm_error', 'detail': 'unrecognised audit result'}


def classify_answer(res, bits, name):
    """classify_write, except that nothing it raises may keep the answer out of the journal: the write has already happened."""
    try:
        return classify.classify_write(res, bits, name)
    except Exception as exc:  # noqa: BLE001
        return {'state': 'unknown', 'reason': 'vm_error', 'detail': 'the answer could not be classified (%s)' % type(exc).__name__}


def gate_accepted(outcome, post):
    """The write was answered `accepted`; the check made right after it decides whether that stands.  One rule for the live path and for a
    restart that finds the check on disk (F08).  Anything but `accepted` is returned as it is."""
    if outcome['state'] != 'accepted':
        return outcome
    if post.get('read_failed'):
        return {'state': 'unknown', 'reason': 'runtime_unreachable', 'detail': post.get('detail') or 'post-check reads failed'}
    if post.get('identity_changed'):
        return {'state': 'unknown', 'reason': 'runtime_restarted'}
    if post.get('latched_after'):
        return {'state': 'unknown', 'reason': 'latched_after_write'}
    if not post.get('scan'):
        return {'state': 'unknown', 'reason': 'runtime_unreachable', 'detail': 'post-check gave no scan-clock marker'}
    return outcome


class Engine:
    def __init__(self, cfg, registry, keys, journal, client, trace, hooks=None, clock=now_ms, sleep=time.sleep):
        self.cfg, self.registry, self.keys = cfg, registry, keys
        self.journal, self.client, self.trace = journal, client, trace
        self.hooks, self.clock, self.sleep = hooks or NoHooks(), clock, sleep
        self._unauth_window = []
        self.counters = {'unauthenticated': 0, 'unattributed_rejects': 0, 'oversize_lines': 0, 'internal_errors': 0,
                         'conflicts': 0, 'requests': 0}
        self._notice_sent = {'unauthenticated': 0, 'unattributed_rejects': 0, 'oversize_lines': 0, 'internal_errors': 0}
        self._notice_at = 0
        self._life = journal.seq
        self._last_request_ms = None
        self._reconcile_failed = set()          # (action_id, pass) whose look raised: not tried again in this process
        self._pending_terminals = {}            # ordered action IDs; the journal, not this queue, owns the result
        for aid, slot in journal.occupied.items():
            if slot['terminal'] is not None:
                event = journal.final_event(aid)
                if event is not None and event.get('event_id') not in trace.seen:
                    self._pending_terminals[aid] = None

    def _hook(self, name):
        fn = getattr(self.hooks, name, None)
        if fn:
            fn()

    def _skip_gate(self, name):
        fn = getattr(self.hooks, 'skip_gate', None)
        return bool(fn and fn(name))

    def _emit(self, event):
        return self.trace.emit(event, anchor=self.journal.head())

    def _publish_terminal(self, aid, event=None):
        """Publish the saved result only. A trace failure must never poison a valid request or change its terminal."""
        if self.journal.failed:
            raise JournalWriteError('the journal has failed; result publication is stopped')
        self._pending_terminals.setdefault(aid, None)
        if event is None:
            event = self.journal.final_event(aid)
        if event is None:
            self._pending_terminals.pop(aid, None)
            return
        try:
            self._emit(event)
        except JournalWriteError:
            raise
        except Exception:  # publication only: the journal already contains the immutable result
            self.counters['internal_errors'] += 1
        else:
            self._pending_terminals.pop(aid, None)

    def retry_terminal_events(self, limit=100):
        """Bounded, fair retries of recorded results; no runtime reads, POSTs, classification or journal appends."""
        if self.journal.failed:
            raise JournalWriteError('the journal has failed; result publication is stopped')
        for aid in list(islice(self._pending_terminals, limit)):
            self._pending_terminals.pop(aid)
            self._publish_terminal(aid)         # a failure goes to the back, so one result cannot starve the rest

    # ---- crash recovery and reconcile ---------------------------------------------------------------------------------
    def recover(self, why='a bridge restart'):
        """Conclude every action that is in flight.  One action's failure is that action's alone: it is concluded from the journal alone (no
        network, nothing sent) and the next one is looked at, so a failure here cannot repeat itself at every start.  Only a journal that
        cannot be written stops it."""
        self.retry_terminal_events()
        for aid in self.journal.in_flight_without_terminal():
            try:
                self._conclude_after_restart(aid, why)
            except JournalWriteError:
                raise
            except Exception as exc:  # noqa: BLE001 - the reason this guard exists
                self.counters['internal_errors'] += 1
                self._conclude_from_journal(aid, why, exc)

    def _conclude_from_journal(self, aid, why, exc):
        """The last resort when concluding an action from IA2 has failed: what the journal alone allows, an honest `unknown` (it may or may not
        have been sent).  It reads nothing from the runtime and sends nothing, so it cannot fail the way the first attempt did.  If even this
        cannot be done -- a record that is not what this bridge writes -- the action stays in flight: its target stays busy (fail closed)
        and the status file counts it."""
        a = self.journal.actions.get(aid)
        if a is None or a['terminal'] is not None:
            return                                   # the failure came after the terminal record: only the trace line may be missing
        try:
            intent = a['intent'] if isinstance(a['intent'], dict) else {}
            resp = a['response'] if isinstance(a.get('response'), dict) else {}
            info = intent['info'] if isinstance(intent.get('info'), dict) else {}
            event = self._event(aid, info, 'unknown', 'runtime_unreachable', registry_sha256=self.registry.sha256,
                                detail='concluding this action after %s failed (%s); concluded from the journal alone, not resent (D8)'
                                       % (why, type(exc).__name__),
                                runtime_id=intent.get('runtime_id'), label=intent.get('label'), sent_at_ms=resp.get('sent_at_ms'))
            self._terminal(aid, event)
        except JournalWriteError:
            raise
        except Exception:  # noqa: BLE001
            self.counters['internal_errors'] += 1

    def _conclude_after_restart(self, aid, why='a bridge restart'):
        a = self.journal.actions[aid]
        intent, resp = a['intent'], a.get('response')
        info = intent['info']
        status, srec = self.client.get_json('/status')
        extra = {'runtime_id': intent.get('runtime_id'), 'label': intent.get('label')}
        schema_error = None
        try:
            runtime_schema.status(status, 'recovery')
        except Reject as exc:
            schema_error = exc.detail
        identity_changed = isinstance(status, dict) and status.get('runtime_id') != intent.get('runtime_id')
        sent_at = None
        if resp is not None:
            outcome = self._response_outcome(aid, intent, resp)
            source, sent_at = 'journal response record', resp.get('sent_at_ms')
        else:
            audit, _ = self.client.get_json('/audit')
            if not isinstance(audit, list):
                event = self._event(aid, info, 'unknown', 'runtime_unreachable',
                                    detail='%s came before this action concluded and /audit could not be read; not resent (D8)' % why, **extra)
                self._terminal(aid, event)
                return
            row = find_audit_row(audit, intent['label'], intent['variable'], intent['bits'])
            if row is None:
                outcome = {'state': 'unknown', 'reason': 'runtime_restarted' if identity_changed else 'bridge_restarted',
                           'detail': 'no /audit entry carries this label (that does not show it was not executed); not resent (D8)'}
            else:
                outcome = outcome_from_audit_row(row, intent['bits'])
            source = '/audit entry' if row is not None else '/audit (no entry)'
        health, _ = self.client.get_json('/health')
        try:
            runtime_schema.health(health)
        except Reject as exc:
            schema_error = schema_error or exc.detail
        if outcome['state'] == 'accepted' and resp is not None:
            outcome = self._gate_with_stored_post(a, intent, outcome)      # what was recorded right after the write comes first (F08)
        if outcome['state'] == 'accepted':
            if schema_error:
                outcome = {'state': 'unknown', 'reason': 'runtime_unreachable', 'detail': schema_error}
            elif identity_changed:
                outcome = {'state': 'unknown', 'reason': 'runtime_restarted'}
            elif runtime_schema.unsafe_after(status, health):
                outcome = {'state': 'unknown', 'reason': 'latched_after_write'}
        applied = outcome.get('applied_bits')
        entry = self.registry.targets.get(info.get('target_key'))
        # The write happened before this read, on the same runtime instance (identity was checked above): the scan clock now is a
        # sound lower bound for the proof.  It is later than the true send, so the read side can only be stricter, never looser.
        snap = status.get('last_snapshot') if isinstance(status, dict) else None
        ts = snap.get('timestamp_us') if isinstance(snap, dict) else None
        marker = {'post_scan_us': ts, 'scan_anchor_ms': srec['t_recv_ms'] - ts // 1000} if type(ts) is int and not identity_changed and not schema_error else {}
        event = self._event(aid, info, outcome['state'], outcome['reason'], registry_sha256=self.registry.sha256, **marker,
                            detail=outcome.get('detail') or 'concluded after %s from the %s; not resent (D8)' % (why, source),
                            runtime_id=intent.get('runtime_id'), label=intent.get('label'), sent_at_ms=sent_at,
                            applied_bits=applied,
                            applied_value=None if applied is None or entry is None else values.decode(entry['type'], applied),
                            undelivered_device=outcome.get('undelivered_device'), also_modified=outcome.get('also_modified'))
        if outcome.get('disable_target'):
            self.journal.append('disable', target_key=info.get('target_key'), reason=outcome['reason'], action_id=aid)
        self._terminal(aid, event)

    def _response_outcome(self, aid, intent, resp):
        """The classification of a stored answer.  Facts written with the record are used if they are valid for this action; a record that
        has facts which cannot be used is `unknown` (never read again from the cut text); a record without facts is a P0-era one."""
        if 'facts' not in resp:
            return facts.legacy_outcome(resp.get('http'), intent['bits'], intent['variable'])
        outcome, why = facts.facts_outcome(resp['facts'], aid, intent)
        return outcome if outcome is not None else facts.untrusted('the facts stored with the answer cannot be used: %s' % why)

    def _gate_with_stored_post(self, a, intent, outcome):
        """Apply the post_check record found on disk to an `accepted` outcome with the rule the live path used.  No record: unchanged (the
        write was answered, the check never made it to disk).  A record that is incomplete or not this action's: not a clean check."""
        kind, post = facts.stored_post(a.get('post_checks', []), intent.get('label'), intent.get('runtime_id'))
        if kind == 'none':
            return outcome
        if kind == 'unusable':
            return {'state': 'unknown', 'reason': 'runtime_unreachable', 'detail': 'the post-write check on disk cannot be used: %s' % post}
        gated = gate_accepted(outcome, post)
        if gated is outcome:
            return outcome
        return dict(gated, detail='the post-write check recorded before the restart found: %s' % (gated.get('detail') or gated['reason']))

    def reconcile_unknowns(self):
        """Look up writes that ended `unknown` after being sent; record what /audit says without changing the status.  Two looks:
        a write that was queued behind a stalled runtime can still land after the first one."""
        now = self.clock()
        for aid, pass_no in self.journal.reconcile_due(now, self.cfg.get('reconcile_recheck_ms', 10000)):
            if (aid, pass_no) in self._reconcile_failed:
                continue
            try:
                self._reconcile_one(aid, pass_no, now)
            except JournalWriteError:
                raise
            except Exception:  # noqa: BLE001 - one action's look must not end the service or touch the status the action already has
                self.counters['internal_errors'] += 1
                self._reconcile_failed.add((aid, pass_no))

    def _reconcile_one(self, aid, pass_no, now):
        a = self.journal.actions[aid]
        intent = a['intent']
        if pass_no == 1 and now < intent['t_ms'] + RECONCILE_AFTER_MS:
            return
        status, _ = self.client.get_json('/status')
        audit, _ = self.client.get_json('/audit')
        try:
            runtime_schema.identity(status)
        except Reject:
            return
        if not isinstance(audit, list):
            return
        row = None
        if status.get('runtime_id') != intent.get('runtime_id'):
            audit_entry, detail = 'unavailable', 'the runtime restarted; /audit was cleared, so nothing can be said'
        else:
            row = find_audit_row(audit, intent['label'], intent['variable'], intent['bits'])
            audit_entry = 'found' if row is not None else 'absent'
            if row is not None:
                detail = ('/audit has an entry for this write: %r%s' % (row.get('result'), ' (it landed AFTER the first look)' if pass_no == 2 else ''))
            elif pass_no == 1:
                detail = ('/audit had no entry for this write at the first look; it will be looked at again. No entry does not '
                          'show that the write was not executed')
            else:
                detail = ('/audit still has no entry for this write at the second look. That does not show it was not executed: '
                          'the request may never have been read, the runtime may have applied it without an entry (it records a '
                          'write only after applying it, and nothing if the connection closed while the write was queued), or '
                          'the 256-entry ring may have turned over')
        landed = True if audit_entry == 'found' else None  # never False: absence is not evidence of non-execution
        self.journal.append('reconcile', action_id=aid, landed=landed, audit_entry=audit_entry, detail=detail, pass_no=pass_no)
        self._emit({'schema': 'ia2.control/v0', 'kind': 'reconcile_note', 'action_id': aid, 'pass_no': pass_no,
                    'final': pass_no == 2 or audit_entry != 'absent',
                    'event_id': sha256_text('%s:reconcile:%d' % (aid, pass_no))[:32], 'session_id': intent['info'].get('session_id'),
                    'device_id': intent['info'].get('device_id'), 'landed': landed, 'audit_entry': audit_entry, 'detail': detail,
                    'audit_result': None if row is None else row.get('result'),
                    'note': 'does not change the action status', 'observed_at_ms': self.clock()})

    # ---- operator controls and visibility ------------------------------------------------------------------------------
    def apply_enables(self):
        """Operator re-enable of a target the bridge disabled (a 404): lines of {"target_key":..., "reason":...} in the enable file.
        Each line is applied once (by hash), through the journal, so the bridge stays the journal's only writer."""
        path = self.cfg.get('enable_path')
        if not path or not os.path.exists(path):
            return
        with open(path, encoding='utf-8', errors='replace') as f:
            lines = [l.strip() for l in f if l.strip()]
        for line in lines:
            h = sha256_text(line)
            if h in self.journal.applied_enables:
                continue
            try:
                obj = strictjson.loads(line)
                key, reason = obj['target_key'], obj['reason']
                ok = isinstance(key, str) and isinstance(reason, str) and reason.strip() and key in self.registry.targets
            except (strictjson.StrictJSONError, KeyError, TypeError):
                ok = False
            if ok:
                self.journal.append('enable', target_key=key, reason=reason[:200], line_sha256=h, by='operator enable file')
            else:
                self.journal.append('enable_ignored', line_sha256=h, detail='not {"target_key": <registered key>, "reason": <text>}')

    def note_oversize(self, length, line_hash):
        self.counters['oversize_lines'] += 1
        if line_hash not in self.journal.seen_unattributed:
            self.journal.append('gate_reject', action_id=None, req_hash=line_hash, reason='bad_request',
                                detail='request line of %d bytes exceeds the limit; not read' % length)

    def status_snapshot(self):
        return {'pid': os.getpid(), 'journal_seq': self.journal.seq, 'in_flight': len(self.journal.in_flight_without_terminal()),
                'disabled_targets': sorted(self.journal.disabled), 'quarantined_lines': len(self.journal.quarantined),
                'pending_trace_results': len(self._pending_terminals),
                'last_request_ms': self._last_request_ms, 'counters': dict(self.counters), 'time_ms': self.clock()}

    def maybe_notice(self):
        """Requests that cannot be attributed to an action get no per-action answer; this is the only place an operator can see them.
        A `bridge_notice` event (counts only, never content) at most once a minute while those counters grow."""
        now = self.clock()
        if now - self._notice_at < NOTICE_EVERY_MS:
            return
        counts = {k: self.counters[k] for k in self._notice_sent}
        if counts == self._notice_sent:
            return
        self._notice_sent, self._notice_at = counts, now
        self._emit({'schema': 'ia2.control/v0', 'kind': 'bridge_notice', 'event_id': sha256_text('notice:%d:%s' % (self._life, json.dumps(counts, sort_keys=True)))[:32],
                    'counts': counts, 'note': 'requests without a trustworthy action_id, oversize lines or internal errors since this bridge started',
                    'observed_at_ms': now})

    # ---- one request -------------------------------------------------------------------------------------------------
    def safe_handle_line(self, raw_line, arrived_ms=None):
        """handle_line behind a guard.  A line that makes the engine raise is recorded (`internal_error`), quarantined by hash --
        a restart re-reads the requests file from the start, so otherwise one poison line would crash-loop the bridge -- and any
        action it left in flight is concluded from what IA2 recorded (and, if that fails too, from the journal alone: `recover`).  Never
        resends.  A journal that cannot be written is the one failure that is NOT absorbed: it ends the handling, and everything after it,
        with JournalWriteError -- nothing may be sent that could not be recorded."""
        line_hash = sha256_text(raw_line)
        if line_hash in self.journal.quarantined:
            return
        if self.journal.failed:
            raise JournalWriteError('the journal has failed (%s); no request is handled' % self.journal.failed)
        try:
            self.handle_line(raw_line, arrived_ms)
        except JournalWriteError:
            raise
        except Exception as exc:  # noqa: BLE001 - this is the point of the guard
            tb = exc.__traceback__
            while tb.tb_next:
                tb = tb.tb_next
            self.counters['internal_errors'] += 1
            self.journal.append('internal_error', line_sha256=line_hash, error=type(exc).__name__, detail=str(exc)[:300],
                                where='%s:%d' % (tb.tb_frame.f_code.co_filename.rsplit('/', 1)[-1], tb.tb_lineno))
            self.recover(why='an internal error')

    def handle_line(self, raw_line, arrived_ms=None):
        t0 = self.clock()
        arrived_ms = t0 if arrived_ms is None else arrived_ms
        try:
            payload_text, key_id = auth.verify_envelope(raw_line, self.keys, t0)
        except Reject as r:
            self._note_unauthenticated(r, raw_line, t0)
            return
        self.counters['requests'] += 1
        self._last_request_ms = t0
        req_hash = sha256_text(payload_text)
        try:
            obj = strictjson.loads(payload_text)
        except strictjson.StrictJSONError as exc:
            self._unattributed(req_hash, 'bad_request', 'payload is not strict JSON: %s' % exc, key_id, None)
            return
        ids = gates.extract_ids(obj)
        aid = ids['action_id']
        if aid is not None and self.journal.has_action(aid):
            self._duplicate(aid, req_hash, ids, key_id)
            return
        try:
            payload = gates.validate_payload(obj)
            label = make_label(payload)
            if self.journal.label_in_use(label, aid):
                raise Reject('conflicting_action_id', 'the label %s is already used by another action' % label)
            entry = gates.check_registry(payload, self.registry, self.journal.disabled)
            bits, expected = values.validate_and_encode(entry, payload['target'])
            gates.check_time(payload, entry, t0, arrived_ms)
            self._throttle(payload, entry, t0)
            gates.check_hold(self.cfg.get('hold_path'))
            nb_hash = self._northbound()
            rt = self._read_runtime(entry)
        except Reject as r:
            self._reject(ids, obj, req_hash, key_id, r)
            return
        self._hook('after_gates')
        self._send(payload, entry, bits, expected, req_hash, key_id, rt, nb_hash)

    # ---- rejections --------------------------------------------------------------------------------------------------
    def _unattributed(self, req_hash, reason, detail, key_id, target_key):
        """A signed request with no trustworthy action_id: journaled once (never per restart), counted, no per-action event."""
        self.counters['unattributed_rejects'] += 1
        if req_hash not in self.journal.seen_unattributed:
            self.journal.append('gate_reject', action_id=None, req_hash=req_hash, reason=reason, detail=detail,
                                key_id=key_id, target_key=target_key)

    def _reject(self, ids, obj, req_hash, key_id, r):
        aid = ids['action_id']
        info = {k: ids.get(k) for k in ('session_id', 'device_id', 'target_key')}
        info.update(requested_value=obj.get('target') if isinstance(obj, dict) and type(obj.get('target')) in (bool, int, float) else None,
                    deadline_ms=obj.get('deadline_ms') if isinstance(obj, dict) and type(obj.get('deadline_ms')) is int else None,
                    requested_at_ms=obj.get('requested_at_ms') if isinstance(obj, dict) and type(obj.get('requested_at_ms')) is int else None)
        if aid is None:
            self._unattributed(req_hash, r.reason, r.detail, key_id, ids['target_key'])
            return
        event = self._event(aid, info, 'rejected', r.reason, detail=r.detail)
        self.journal.append('gate_reject', action_id=aid, req_hash=req_hash, reason=r.reason, detail=r.detail,
                            key_id=key_id, target_key=ids['target_key'], deadline_ms=info['deadline_ms'], event=event)
        self._publish_terminal(aid, event)

    def _duplicate(self, aid, req_hash, ids, key_id):
        if self.journal.request_hash(aid) == req_hash:
            event = self.journal.final_event(aid)
            if event is not None:
                self._publish_terminal(aid, event)  # idempotent: same saved event, written once
            return                                  # still in flight: nothing to say yet
        self.counters['conflicts'] += 1
        self.journal.append('conflict', action_id=aid, req_hash=req_hash, key_id=key_id,
                            reason='conflicting_action_id')
        self._emit(self._notice(aid, ids, 'conflicting_action_id', req_hash))

    def _note_unauthenticated(self, r, raw_line, t0):
        """Unauthenticated lines never enter the dedup table (an attacker could squat on a real action_id).  Rate-limited
        so a flood cannot grow the journal without bound; a line already on record (a restart re-read it) is not recorded again."""
        self.counters['unauthenticated'] += 1
        line_hash = sha256_text(raw_line)
        if line_hash in self.journal.seen_unattributed:
            return
        self._unauth_window = [t for t in self._unauth_window if t > t0 - 60000]
        if len(self._unauth_window) < 100:
            self._unauth_window.append(t0)
            self.journal.append('unauthenticated', reason=r.reason, detail=r.detail, line_sha256=line_hash)

    # ---- gates that need the journal or the network --------------------------------------------------------------------
    def _throttle(self, payload, entry, now):
        if self.journal.open_for_target(entry['key'], now, entry['single_flight_ms']):
            raise Reject('busy', 'the target has an unfinished action')
        if len(self.journal.intent_times(now - 60000)) >= GLOBAL_PER_MIN:
            raise Reject('rate_limited', 'bridge-wide write limit reached')
        mine = self.journal.intent_times(now - 60000, entry['key'])
        if len(mine) >= entry['max_rate_per_min']:
            raise Reject('rate_limited', 'per-target rate limit reached')
        if mine and now - max(mine) < entry['min_interval_ms']:
            raise Reject('rate_limited', 'per-target minimum interval not elapsed')

    def _northbound(self):
        project_dir = self.cfg.get('project_dir')
        return northbound_guard.check(project_dir) if project_dir else 'unchecked'

    def _read_runtime(self, entry):
        """Two /status reads and one /health read; then G8 and G9.  A first read that fails ends it: no point in waiting for more."""
        s1, r1 = self.client.get_json('/status')
        runtime_schema.status(s1, entry=entry)
        h, rh = self.client.get_json('/health')
        runtime_schema.health(h, entry['require_fieldbus_healthy'])
        # The published scan_count moves in steps of ~10 scans about every 109 ms (the snapshot task's cadence, measured),
        # not on every scan, so "it advanced" can only be asked as "within advance_wait_ms".
        give_up = r1['t_send_ms'] + self.cfg['advance_wait_ms']
        s2, r2 = None, r1
        while s1 is not None and h is not None:
            wait = self.cfg['poll_ms'] / 1000.0 - (self.clock() - r2['t_send_ms']) / 1000.0
            if wait > 0:
                self.sleep(wait)
            s2, r2 = self.client.get_json('/status')
            runtime_schema.status(s2, entry=entry)
            if s2 is None or s2.get('scan_count') != s1.get('scan_count') or self.clock() >= give_up:
                break
        if s1 is None or h is None or s2 is None:
            raise Reject('runtime_unreachable', 'could not read /status and /health')
        skip = self._skip_gate('G8')
        runtime_id = gates.check_runtime(h, s1, s2, skip_g8=skip)
        gates.check_target(h, s2, entry)
        return {'runtime_id': runtime_id, 'scan_count': [s1.get('scan_count'), s2.get('scan_count')],
                'health_sha256': sha256_text(rh['text']), 'status_sha256': sha256_text(r2['text']), 'g8_skipped': skip}

    # ---- send --------------------------------------------------------------------------------------------------------
    def _send(self, payload, entry, bits, expected, req_hash, key_id, rt, nb_hash):
        aid = payload['action_id']
        label = make_label(payload)
        info = {'session_id': payload['session_id'], 'device_id': payload['device_id'],
                'target_key': payload['target_key'], 'requested_value': payload['target'],
                'deadline_ms': payload['deadline_ms'], 'requested_at_ms': payload['requested_at_ms']}
        self.journal.append('intent', action_id=aid, req_hash=req_hash, target_key=entry['key'],
                            deadline_ms=payload['deadline_ms'], variable=entry['ia2_variable'], bits=bits,
                            label=label, runtime_id=rt['runtime_id'], readings=rt, northbound=nb_hash,
                            key_id=key_id, info=info)
        self._hook('after_intent')
        sent_at = self.clock()
        res = self.client.request('POST', '/write', {'name': entry['ia2_variable'], 'value': bits}, origin=label)
        response_at = self.clock()
        self._hook('after_http')
        # Classified from the FULL answer and stored with it in ONE record: a restart decides on these facts, never on the cut diagnostic text.
        outcome = classify_answer(res, bits, entry['ia2_variable'])
        self.journal.append('response', action_id=aid, http=journal_view(res),
                            facts=facts.response_facts(aid, label, entry['ia2_variable'], bits, res, outcome),
                            sent_at_ms=sent_at, response_at_ms=response_at)
        self._hook('after_response')
        post = self._post_check(res, label, entry, bits, rt['runtime_id'])
        self.journal.append('post_check', action_id=aid, v=facts.POST_V, label=label, runtime_id=rt['runtime_id'], **post)
        self._hook('after_post_check')
        outcome = gate_accepted(outcome, post)
        if outcome.get('disable_target'):
            self.journal.append('disable', target_key=entry['key'], reason=outcome['reason'], action_id=aid)
        applied = outcome.get('applied_bits')
        scan = post.get('scan') or {}
        event = self._event(aid, info, outcome['state'], outcome['reason'], detail=outcome.get('detail'),
                            runtime_id=rt['runtime_id'], label=label, sent_at_ms=sent_at, response_at_ms=response_at,
                            applied_bits=applied,
                            applied_value=None if applied is None else values.decode(entry['type'], applied),
                            undelivered_device=outcome.get('undelivered_device'),
                            also_modified=outcome.get('also_modified'), audit_match=post.get('audit_match'),
                            post_scan_us=scan.get('ts_us'), scan_anchor_ms=scan.get('anchor_ms'),
                            registry_sha256=self.registry.sha256)
        self._terminal(aid, event)

    def _post_check(self, res, label, entry, bits, runtime_id):
        if res['kind'] != 'response':
            return {'skipped': 'the write got no HTTP answer; the runtime may be unreachable', 'read_failed': False}
        status, rs = self.client.get_json('/status')
        health, _ = self.client.get_json('/health')
        audit, _ = self.client.get_json('/audit')
        out = {'read_failed': False}
        try:
            runtime_schema.status(status, 'post')
            runtime_schema.health(health)
        except Reject as exc:
            out.update(read_failed=True, detail=exc.detail)
        else:
            out['identity_changed'] = status['runtime_id'] != runtime_id
            out['latched_after'] = runtime_schema.unsafe_after(status, health)
            ts = status['last_snapshot']['timestamp_us']
            out['scan'] = {'ts_us': ts, 'anchor_ms': rs['t_recv_ms'] - ts // 1000}
        row = find_audit_row(audit, label, entry['ia2_variable'], bits)
        if isinstance(audit, list):
            out['audit_match'] = ({'found': True, 'result': row.get('result'), 'applied': row.get('applied'),
                                   'ts_unix_secs': row.get('ts_unix_secs'),
                                   'count': len([e for e in audit if isinstance(e, dict) and e.get('origin') == label])}
                                  if row is not None else {'found': False})
        return out

    # ---- trace events --------------------------------------------------------------------------------------------------
    def _terminal(self, aid, event):
        self.journal.append('terminal', action_id=aid, event=event)
        self._publish_terminal(aid, event)

    def _event(self, aid, info, status, reason, **extra):
        event = {'schema': 'ia2.control/v0', 'kind': 'action_status', 'action_id': aid,
                 'event_id': sha256_text('%s:%s' % (aid, status))[:32],
                 'session_id': info.get('session_id'), 'device_id': info.get('device_id'),
                 'target_key': info.get('target_key'), 'requested_value': info.get('requested_value'),
                 'deadline_ms': info.get('deadline_ms'), 'requested_at_ms': info.get('requested_at_ms'),
                 'status': status, 'reason': reason, 'claim': CLAIMS[status]['claim'],
                 'not_claimed': CLAIMS[status]['not_claimed'], 'observed_at_ms': self.clock()}
        event.update({k: v for k, v in extra.items() if v is not None})
        # JSON permits exponent syntax that overflows a parser's float. Keep
        # rejection events signable as strict JSON; the reason still reports
        # bad_value, and the original request bytes remain in the journal.
        for name in ('requested_value', 'applied_value'):
            value = event.get(name)
            if isinstance(value, float) and not math.isfinite(value):
                event[name] = None
        return event

    def _notice(self, aid, ids, reason, req_hash):
        return {'schema': 'ia2.control/v0', 'kind': 'conflict_notice', 'action_id': aid, 'status': 'rejected',
                'event_id': sha256_text('%s:conflict:%s' % (aid, req_hash))[:32], 'reason': reason,
                'note': 'a different request reused this action_id; the original action is unchanged',
                'session_id': ids.get('session_id'), 'device_id': ids.get('device_id'),
                'observed_at_ms': self.clock()}
