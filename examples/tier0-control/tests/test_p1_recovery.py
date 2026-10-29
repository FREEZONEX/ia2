"""P1 F08 / F09: after a restart the bridge decides on what the journal holds -- the facts stored with the answer and the post-write check --
and never on a diagnostic copy of the text or on a health reading that is better than what was recorded.

A crash is simulated the way it leaves the disk: a real journal is cut right after the last record of a kind (the terminal, or the
post_check and terminal, ...), so the same test runs against the P0 code and the P1 code.  `strip` rewrites a journal as the P0 bridge
wrote it.  Every case ends with the same two facts: the runtime saw exactly one POST, and the action has exactly one terminal record."""
import hashlib
import json
import sys
import unittest
from pathlib import Path
import tempfile

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, Client, close, engine, envelope, payload, reg_for
from bridge import strictjson
from bridge.journal import DAY_MS, GENESIS
import test_verifier as tv
import verifier

START = 1800000000000
CLEAR = {'pending': [], 'acknowledged': [], 'unconfirmed': []}


class Crash(Exception):
    pass


def stop():
    raise Crash()


class RecClient(Client):
    """p0_support.Client plus: field overrides on /status and /health once a POST has happened (`after`), and a scripted answer to
    POST /write (`answer(res, body) -> res`)."""

    def __init__(self, clock):
        super().__init__(clock)
        self.after = {}
        self.answer = None

    def get_json(self, path):
        body, rec = super().get_json(path)
        if self.posts and isinstance(body, dict) and path in self.after:
            body = dict(body, **self.after[path])
            rec = dict(rec, text=json.dumps(body))
        return body, rec

    def request(self, method, path, body=None, origin=None):
        res = super().request(method, path, body, origin)
        return self.answer(res, body) if self.answer else res


# how the runtime looks AFTER the write -> what the live path concludes
NEGATIVE = {
    'latch_reason in /status': ({'/status': {'latch_reason': 'scan watchdog'}}, ('unknown', 'latched_after_write')),
    'fault in /status': ({'/status': {'fault': 'vm'}}, ('unknown', 'latched_after_write')),
    'watchdog tripped in /health': ({'/health': {'watchdog_tripped': True}}, ('unknown', 'latched_after_write')),
    'health not ok': ({'/health': {'status': 'degraded'}}, ('unknown', 'latched_after_write')),
    'snapshot tap stopped': ({'/health': {'snapshot_tap': {'running': False}}}, ('unknown', 'latched_after_write')),
    'failsafe pending in /health': ({'/health': {'failsafe_status': dict(CLEAR, pending=['dev'])}}, ('unknown', 'latched_after_write')),
    'failsafe unconfirmed in /status': ({'/status': {'failsafe_status': dict(CLEAR, unconfirmed=['dev'])}}, ('unknown', 'latched_after_write')),
    'another runtime instance': ({'/status': {'runtime_id': 'R2'}}, ('unknown', 'runtime_restarted')),
    'reads fail (not a status object)': ('bad', ('unknown', 'runtime_unreachable')),
}
ELAPSED = (0, DAY_MS - 1, DAY_MS + 1, 10 * DAY_MS)


def configure(client, how):
    if how == 'bad':
        client.bad_after_post = True
    elif how:
        client.after = how


def heal(client):
    client.after, client.bad_after_post = {}, False


def jlines(tmp):
    return (Path(tmp) / 'journal.jsonl').read_bytes().split(b'\n')[:-1]


def kinds(tmp):
    return [json.loads(l)['kind'] for l in jlines(tmp)]


def cut_after(tmp, kind):
    """Keep the journal up to and including its LAST record of `kind`: what a crash right after that record leaves on disk."""
    lines = jlines(tmp)
    last = max(i for i, l in enumerate(lines) if json.loads(l)['kind'] == kind)
    (Path(tmp) / 'journal.jsonl').write_bytes(b'\n'.join(lines[:last + 1]) + b'\n')
    (Path(tmp) / 'trace.jsonl').unlink(missing_ok=True)             # the terminal event was never written either


def rewrite(tmp, fn):
    """fn(record) -> a record, a list of records, or None (drop); the journal is chained again, so it is valid for the next start."""
    out, prev = [], GENESIS
    for raw in jlines(tmp):
        recs = fn(json.loads(raw))
        for rec in ([] if recs is None else recs if isinstance(recs, list) else [recs]):
            rec = dict(rec, seq=len(out) + 1, prev=prev)
            line = json.dumps(rec, sort_keys=True, separators=(',', ':'), ensure_ascii=True).encode('ascii')
            out.append(line)
            prev = hashlib.sha256(line).hexdigest()
    (Path(tmp) / 'journal.jsonl').write_bytes(b'\n'.join(out) + b'\n')


def as_p0(rec):
    """The record as the P0 bridge wrote it: no facts in the response, no version / label / runtime in the post_check."""
    rec = dict(rec)
    if rec['kind'] == 'response':
        rec.pop('facts', None)
    elif rec['kind'] == 'post_check':
        for k in ('v', 'label', 'runtime_id'):
            rec.pop(k, None)
    return rec


def edit(kind, **changes):
    """A rewrite function that changes (value) or removes (the marker REMOVE) fields of the records of one kind."""
    def fn(rec):
        if rec['kind'] == kind:
            rec = dict(rec)
            for k, v in changes.items():
                if v is REMOVE:
                    rec.pop(k, None)
                else:
                    rec[k] = v
        return rec
    return fn


REMOVE = object()


def sized_body(length, filler='x', junk=False, name='sp', value=1):
    """A JSON answer of exactly `length` characters (plus one trailing junk character that makes the whole not parse, if `junk`)."""
    head, tail = '{"ok":true,"name":"%s","value":%d,"undelivered_device":null,"pad":"' % (name, value), '"}'
    text = head + filler * (length - len(head) - len(tail)) + tail
    assert len(text) == length
    return text + ('x' if junk else '')


def answer_text(text, status=200):
    """The runtime answers with this text; the client has parsed it as strictly as the real one does."""
    def fn(res, body):
        try:
            parsed = strictjson.loads(text) if text else None
        except strictjson.StrictJSONError:
            parsed = None
        return dict(res, status=status, text=text, json=parsed)
    return fn


def good_sample(ev):
    s = tv.sample(t=ev.get('post_scan_us', 0) + 200_000, value=1, rid=ev.get('runtime_id'))
    s['variables'][0]['name'] = 'fb'
    return s


def read_side(ev):
    """What the independent verifier makes of the event given a perfect feedback sample after the write."""
    entry = dict(reg_for().targets['set'], verify={'feedback': 'fb', 'tolerance': 0, 'settle_ms': 5000})
    return verifier.derive(AID, entry, [ev], [good_sample(ev)], ev.get('scan_anchor_ms', 0) + (ev.get('post_scan_us', 0) + 300_000) // 1000)


class Recovery(unittest.TestCase):
    def play(self, how=None, boundary='post_check', elapsed=0, restarts=1, strip=False, answer=None, after_cut=None, heal_first=True):
        """One live run on a fresh journal, a crash at `boundary`, then `restarts` restarts that each recover.  Returns (live event, [event seen
        after each restart], client, directory name)."""
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = RecClient(clock)
            configure(client, how)
            client.answer = answer
            e = engine(tmp, clock, client=client)
            line = envelope(payload(clock[0]))
            e.handle_line(line)
            live = e.journal.final_event(AID)
            close(e)
            if boundary != 'terminal':
                cut_after(tmp, boundary)
            if strip:
                rewrite(tmp, as_p0)
            if after_cut:
                after_cut(tmp)
            if heal_first:
                heal(client)
            clock[0] += elapsed
            seen = []
            for _ in range(restarts):
                e = engine(tmp, clock, client=client)
                try:
                    e.recover()
                    seen.append(e.journal.final_event(AID))
                    e.handle_line(line)                              # the same request again: the recorded result, nothing new
                except Exception as exc:  # noqa: BLE001
                    raise AssertionError('the restart raised %r instead of concluding the action' % (exc,)) from None
                self.assertEqual(e.journal.final_event(AID), seen[-1])
                close(e)
            self.assertEqual(len(client.posts), 1, 'exactly one POST for the action, however many restarts')
            self.assertEqual([k for k in kinds(tmp) if k == 'terminal'], ['terminal'], 'exactly one terminal record')
            return live, seen, client

    # ---- F08 -------------------------------------------------------------------------------------------------------------------
    def test_F08_01_the_negative_fact_recorded_after_the_write_survives_a_restart_that_looks_healthy(self):
        for name, (how, expected) in NEGATIVE.items():
            with self.subTest(evidence=name):
                live, seen, _ = self.play(how)
                self.assertEqual((live['status'], live['reason']), expected, 'precondition: the live path concludes this')
                self.assertEqual((seen[0]['status'], seen[0]['reason']), expected)
                self.assertEqual(read_side(seen[0])['status'], 'unknown', 'a perfect feedback afterwards must not make it verified')

    def test_F08_02_every_sync_boundary_every_age_every_restart_count(self):
        for name, (how, expected) in NEGATIVE.items():
            for elapsed in ELAPSED:
                with self.subTest(evidence=name, elapsed=elapsed):
                    live, seen, _ = self.play(how, elapsed=elapsed, restarts=3)
                    for ev in seen:
                        self.assertEqual((ev['status'], ev['reason']), expected)
                    self.assertEqual({e['event_id'] for e in seen}, {seen[0]['event_id']}, 'three restarts, one and the same terminal')

    def test_F08_03_a_journal_from_the_p0_bridge_keeps_its_negative_facts_too(self):
        for name, (how, expected) in NEGATIVE.items():
            with self.subTest(evidence=name):
                _, seen, _ = self.play(how, strip=True, elapsed=DAY_MS + 1)
                self.assertEqual((seen[0]['status'], seen[0]['reason']), expected)

    def test_F08_04_recovery_equals_the_live_decision_for_the_same_prefix(self):
        for name, (how, expected) in list(NEGATIVE.items()) + [('healthy', (None, ('accepted', None)))]:
            for strip in (False, True):
                with self.subTest(evidence=name, p0_records=strip):
                    live, seen, _ = self.play(how, strip=strip)
                    self.assertEqual((live['status'], live['reason']), (seen[0]['status'], seen[0]['reason']))

    def test_F08_05_healthy_complete_evidence_is_still_accepted_and_verifiable(self):
        for strip in (False, True):
            for elapsed in ELAPSED:
                with self.subTest(p0_records=strip, elapsed=elapsed):
                    _, seen, _ = self.play(None, strip=strip, elapsed=elapsed, restarts=2)
                    self.assertEqual((seen[0]['status'], seen[0]['reason']), ('accepted', None))
                    # after the action's deadline the read side must not verify anything, however perfect the feedback
                    self.assertEqual(read_side(seen[0])['status'], 'verified' if elapsed == 0 else 'unknown')

    def test_F08_06_an_incomplete_or_foreign_post_check_is_not_a_clean_check(self):
        scan_ok = {'ts_us': 5, 'anchor_ms': 7}
        broken = {
            'no scan marker': edit('post_check', scan=REMOVE), 'scan is null': edit('post_check', scan=None),
            'scan ts is text': edit('post_check', scan={'ts_us': '5', 'anchor_ms': 7}),
            'scan has no anchor': edit('post_check', scan={'ts_us': 5}), 'read_failed missing': edit('post_check', read_failed=REMOVE),
            'read_failed is text': edit('post_check', read_failed='false'), 'identity_changed missing': edit('post_check', identity_changed=REMOVE),
            'latched_after is null': edit('post_check', latched_after=None), 'unknown version': edit('post_check', v=2),
            'version is a bool': edit('post_check', v=True),
            'another label': edit('post_check', label='tier0.other.00000000'), 'another runtime': edit('post_check', runtime_id='R9'),
            'a skipped check': edit('post_check', skipped='no answer'), 'sane control': edit('post_check', scan=scan_ok),
        }
        for name, fn in broken.items():
            with self.subTest(post_check=name):
                _, seen, _ = self.play(None, after_cut=lambda tmp, fn=fn: rewrite(tmp, fn))
                if name == 'sane control':
                    self.assertEqual(seen[0]['status'], 'accepted', 'a complete record that merely carries other numbers is fine')
                else:
                    self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'runtime_unreachable'))
                    self.assertIn('post-write check', seen[0]['detail'])

    def test_F08_07_two_post_check_records_are_ambiguous(self):
        twice = lambda tmp: rewrite(tmp, lambda rec: [rec, rec] if rec['kind'] == 'post_check' else rec)
        _, seen, _ = self.play(None, after_cut=twice)
        self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'runtime_unreachable'))
        self.assertIn('more than one', seen[0]['detail'])

    def test_F08_08_no_post_check_record_means_the_check_never_reached_the_disk(self):
        # crash between the answer and the check: nothing negative is on record, so the contract's conservative recovery applies ...
        _, seen, _ = self.play(None, boundary='response')
        self.assertEqual((seen[0]['status'], seen[0]['reason']), ('accepted', None))
        self.assertIn('not resent', seen[0]['detail'])
        # ... and a runtime that is latched NOW still limits it: what is read after the restart can only add limits
        _, seen, _ = self.play({'/status': {'latch_reason': 'scan watchdog'}}, boundary='response', heal_first=False)
        self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'latched_after_write'))

    def test_F08_09_definite_answers_keep_their_state_whatever_the_post_check_says(self):
        modified = lambda res, body: dict(res, json=dict(res['json'], value=body['value'] + 1), text=json.dumps(dict(res['json'], value=body['value'] + 1)))
        undelivered = lambda res, body: dict(res, json=dict(res['json'], undelivered_device='dev'), text=json.dumps(dict(res['json'], undelivered_device='dev')))
        denied = answer_text('rejected by project governance', status=403)
        for name, answer, expected in (('modified', modified, ('modified', 'clamped')), ('undelivered', undelivered, ('undelivered', 'link_down')),
                                       ('rejected', denied, ('rejected', 'governance_denied'))):
            for how in (None, {'/status': {'latch_reason': 'scan watchdog'}}):
                for strip in (False, True):
                    with self.subTest(answer=name, latched=bool(how), p0_records=strip):
                        live, seen, _ = self.play(how, answer=answer, strip=strip)
                        self.assertEqual((live['status'], live['reason']), expected)
                        self.assertEqual((seen[0]['status'], seen[0]['reason']), expected)

    def test_F08_10_a_terminal_already_on_disk_is_kept_as_it_is(self):
        live, seen, _ = self.play(None, boundary='terminal', heal_first=False, restarts=3)
        self.assertEqual(live['status'], 'accepted')
        self.assertTrue(all(ev == live for ev in seen))
        live, seen, _ = self.play({'/status': {'latch_reason': 'scan watchdog'}}, boundary='terminal', heal_first=True, restarts=2)
        self.assertTrue(all(ev == live for ev in seen) and live['status'] == 'unknown')

    def test_F08_11_the_crash_hook_after_the_post_check_stops_with_the_record_on_disk(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = RecClient(clock)
            e = engine(tmp, clock, client=client)
            e.hooks = type('Hooks', (), {'after_post_check': staticmethod(stop)})()
            with self.assertRaises(Crash):
                e.handle_line(envelope(payload(clock[0])))
            close(e)
            self.assertEqual(kinds(tmp), ['intent', 'response', 'post_check'])
            rec = json.loads(jlines(tmp)[-1])
            self.assertEqual((rec.get('v'), rec.get('label'), rec.get('runtime_id')), (1, e.journal.actions[AID]['intent']['label'], 'R'))
            e = engine(tmp, clock, client=client)
            e.recover()
            self.assertEqual(e.journal.final_event(AID)['status'], 'accepted')
            self.assertEqual(len(client.posts), 1)
            close(e)

    def test_F08_12_a_second_interruption_before_the_terminal_is_written_leads_to_the_same_conclusion(self):
        class Killed(BaseException):                      # a kill, not an error: nothing in the engine may absorb it
            pass
        for name, (how, expected) in list(NEGATIVE.items()) + [('healthy', (None, ('accepted', None)))]:
            for strip in (False, True):
                with self.subTest(evidence=name, p0_records=strip), tempfile.TemporaryDirectory() as tmp:
                    clock = [START]
                    client = RecClient(clock)
                    configure(client, how)
                    e = engine(tmp, clock, client=client)
                    line = envelope(payload(clock[0]))
                    e.handle_line(line)
                    close(e)
                    cut_after(tmp, 'post_check')
                    if strip:
                        rewrite(tmp, as_p0)
                    heal(client)
                    # the restart is killed at the moment it would write the terminal ...
                    e = engine(tmp, clock, client=client)

                    def killed(aid, event):
                        raise Killed()
                    e._terminal = killed
                    with self.assertRaises(Killed):
                        e.recover()
                    self.assertIsNone(e.journal.final_event(AID))
                    close(e)
                    self.assertNotIn('terminal', kinds(tmp))
                    # ... and the next one decides the same
                    e = engine(tmp, clock, client=client)
                    e.recover()
                    ev = e.journal.final_event(AID)
                    self.assertEqual((ev['status'], ev['reason']), expected)
                    close(e)
                    self.assertEqual(len(client.posts), 1)
                    self.assertEqual(kinds(tmp).count('terminal'), 1)

    def test_the_intent_alone_recovers_from_the_audit_and_never_resends(self):
        for elapsed in (0, DAY_MS + 1):
            with self.subTest(elapsed=elapsed):
                _, seen, _ = self.play(None, boundary='intent', elapsed=elapsed)
                self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'bridge_restarted'))

    # ---- F09 -------------------------------------------------------------------------------------------------------------------
    def test_F09_01_a_prefix_that_parses_is_not_evidence(self):
        for filler in ('x', '界'):
            with self.subTest(filler=filler):
                # 4096 characters that are a complete JSON answer, then one more character: the whole answer does not parse
                text = sized_body(4096, filler, junk=True)
                live, seen, _ = self.play(None, boundary='response', answer=answer_text(text))
                self.assertEqual((live['status'], live['reason']), ('unknown', 'vm_error'))
                self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'vm_error'))
                self.assertEqual(read_side(seen[0])['status'], 'unknown')

    def test_F09_02_the_boundaries_of_the_stored_copy(self):
        for length in (4095, 4096, 4097):
            for filler in ('x', '界'):
                for junk in (False, True):
                    with self.subTest(chars=length, filler=filler, whole_answer_parses=not junk):
                        text = sized_body(length, filler, junk=junk)
                        for boundary in ('response', 'post_check'):
                            live, seen, _ = self.play(None, boundary=boundary, answer=answer_text(text), elapsed=DAY_MS + 1)
                            want = ('unknown', 'vm_error') if junk else ('accepted', None)
                            self.assertEqual((live['status'], live['reason']), want, 'precondition')
                            self.assertEqual((seen[0]['status'], seen[0]['reason']), want)

    def test_F09_03_a_long_valid_answer_is_still_accepted_after_a_restart(self):
        text = sized_body(9000)
        live, seen, _ = self.play(None, boundary='post_check', answer=answer_text(text))
        self.assertEqual(live['status'], 'accepted')
        self.assertEqual(seen[0]['status'], 'accepted')
        self.assertEqual(read_side(seen[0])['status'], 'verified')

    def test_F09_04_the_stored_text_is_cut_at_4096_characters_but_the_facts_say_what_was_whole(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = RecClient(clock)
            client.answer = answer_text(sized_body(4097, junk=False))
            e = engine(tmp, clock, client=client)
            e.handle_line(envelope(payload(clock[0])))
            close(e)
            resp = next(json.loads(l) for l in jlines(tmp) if json.loads(l)['kind'] == 'response')
            self.assertTrue(resp['http']['text_truncated'])
            self.assertEqual(len(resp['http']['text']), 4096)
            facts = resp.get('facts') or {}
            self.assertEqual(facts.get('body_chars'), 4097)
            self.assertEqual((facts.get('outcome') or {}).get('state'), 'accepted')
            self.assertEqual((facts.get('action_id'), facts.get('variable'), facts.get('bits')), (AID, 'sp', 1))

    def test_F09_05_records_from_the_p0_bridge(self):
        # (text length, junk) -> what a restart may conclude when the record has no facts: only an untruncated, parsing text counts
        for length, junk, want in ((4095, False, 'accepted'), (4096, False, 'accepted'), (4097, False, 'unknown'),
                                   (4095, True, 'unknown'), (4096, True, 'unknown'), (4097, True, 'unknown')):
            with self.subTest(chars=length, whole_answer_parses=not junk):
                text = sized_body(length, junk=junk)
                _, seen, _ = self.play(None, boundary='post_check', answer=answer_text(text), strip=True)
                self.assertEqual(seen[0]['status'], want)
                if want == 'unknown':
                    self.assertEqual(seen[0]['reason'], 'vm_error')

    def test_F09_06_p0_records_with_a_missing_wrong_or_false_marker_are_not_evidence(self):
        good = answer_text(sized_body(300))
        cases = {
            'marker missing': (lambda r: self._http_edit(r, text_truncated=REMOVE), 'unknown'),
            'marker is the text "false"': (lambda r: self._http_edit(r, text_truncated='false'), 'unknown'),
            'marker is null': (lambda r: self._http_edit(r, text_truncated=None), 'unknown'),
            'marker is 0': (lambda r: self._http_edit(r, text_truncated=0), 'unknown'),
            'marker is true': (lambda r: self._http_edit(r, text_truncated=True), 'unknown'),
            'marker is false': (lambda r: self._http_edit(r, text_truncated=False), 'accepted'),
            'no text at all': (lambda r: self._http_edit(r, text=REMOVE), 'unknown'),
            'text is not text': (lambda r: self._http_edit(r, text=5), 'unknown'),
            'status is text': (lambda r: self._http_edit(r, status='200'), 'unknown'),
            'kind is unknown': (lambda r: self._http_edit(r, kind='other'), 'unknown'),
        }
        for name, (fn, want) in cases.items():
            with self.subTest(record=name):
                def both(tmp, fn=fn):
                    rewrite(tmp, lambda rec: fn(as_p0(rec)) if rec['kind'] == 'response' else as_p0(rec))
                _, seen, _ = self.play(None, boundary='post_check', answer=good, after_cut=both)
                self.assertEqual(seen[0]['status'], want)

    @staticmethod
    def _http_edit(rec, **changes):
        http = dict(rec['http'])
        for k, v in changes.items():
            if v is REMOVE:
                http.pop(k, None)
            else:
                http[k] = v
        return dict(rec, http=http)

    def test_F09_07_a_p0_record_of_a_failed_transfer_or_a_refusal_needs_no_body(self):
        def error_record(tmp):
            def fn(rec):
                if rec['kind'] == 'response':
                    return dict(as_p0(rec), http={'kind': 'error', 'error_kind': 'timeout', 'method': 'POST', 'path': '/write'})
                return as_p0(rec)
            rewrite(tmp, fn)
        _, seen, _ = self.play(None, boundary='response', after_cut=error_record)
        self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'runtime_unreachable'))
        _, seen, _ = self.play(None, boundary='response', answer=answer_text('rejected by project governance', status=403), strip=True)
        self.assertEqual((seen[0]['status'], seen[0]['reason']), ('rejected', 'governance_denied'))

    def test_F09_08_facts_that_do_not_belong_to_the_action_are_not_used_and_the_text_is_not_a_fallback(self):
        # the stored text below is a perfectly good `accepted` answer: with unusable facts a restart must still not trust it
        tampered = {
            'another action': {'action_id': 'f' * 32}, 'another label': {'label': 'tier0.other.00000000'}, 'another variable': {'variable': 'other'},
            'other bits': {'bits': 2}, 'bits as text': {'bits': '1'}, 'unknown version': {'v': 2}, 'version is true': {'v': True},
            'no outcome': {'outcome': REMOVE}, 'outcome is a list': {'outcome': []}, 'unknown state': {'outcome': {'state': 'verified', 'reason': None}},
            'a state the bridge never writes, with a valid reason': {'outcome': {'state': 'verified', 'reason': 'no_feedback'}},
            'a pre-terminal state, with a valid reason': {'outcome': {'state': 'requested', 'reason': 'no_feedback'}},
            'accepted without bits': {'outcome': {'state': 'accepted', 'reason': None}},
            'unknown without a reason': {'outcome': {'state': 'unknown', 'reason': None}},
            'a reason nobody defined': {'outcome': {'state': 'unknown', 'reason': 'because'}},
            'an extra field': {'outcome': {'state': 'accepted', 'reason': None, 'applied_bits': 1, 'verified': True}},
            'undelivered without a device': {'outcome': {'state': 'undelivered', 'reason': 'link_down', 'applied_bits': 1}},
        }
        for name, changes in tampered.items():
            with self.subTest(facts=name):
                def fn(rec, changes=changes):
                    if rec['kind'] == 'response':
                        facts = dict(rec.get('facts') or {})          # (a P0 record has none: the tampered ones are then simply added)
                        for k, v in changes.items():
                            if v is REMOVE:
                                facts.pop(k, None)
                            else:
                                facts[k] = v
                        rec = dict(rec, facts=facts)
                    return rec
                _, seen, _ = self.play(None, boundary='post_check', after_cut=lambda tmp, fn=fn: rewrite(tmp, fn))
                self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'vm_error'))
                self.assertIn('facts', seen[0]['detail'])
        with self.subTest(facts='not an object'):
            _, seen, _ = self.play(None, boundary='post_check', after_cut=lambda tmp: rewrite(tmp, edit('response', facts='accepted')))
            self.assertEqual((seen[0]['status'], seen[0]['reason']), ('unknown', 'vm_error'))

    def test_F09_09_facts_and_post_check_together(self):
        answers = {'whole answer is good': answer_text(sized_body(300)), 'whole answer does not parse': answer_text(sized_body(300, junk=True))}
        posts = {'healthy': None, 'latched': {'/status': {'latch_reason': 'scan watchdog'}}}
        for a_name, answer in answers.items():
            for p_name, how in posts.items():
                for boundary in ('response', 'post_check'):
                    for strip in (False, True):
                        for elapsed in (0, DAY_MS + 1):
                            with self.subTest(answer=a_name, post_check=p_name, crash_after=boundary, p0_records=strip, elapsed=elapsed):
                                _, seen, _ = self.play(how, boundary=boundary, answer=answer, strip=strip, elapsed=elapsed, restarts=2)
                                if a_name.endswith('not parse'):
                                    want = ('unknown', 'vm_error')                      # the answer decides first, whatever the check says
                                elif p_name == 'latched' and boundary == 'post_check':
                                    want = ('unknown', 'latched_after_write')          # the check is on disk: it limits the answer
                                else:
                                    want = ('accepted', None)                          # healthy, or the check never reached the disk and the runtime looks healthy now
                                self.assertEqual((seen[0]['status'], seen[0]['reason']), want)
                                self.assertEqual(seen[0], seen[1])


if __name__ == '__main__':
    unittest.main()
