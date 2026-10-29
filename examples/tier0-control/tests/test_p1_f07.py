"""P1 F07: an unexpected failure while an action is being finished must not end the service, must not send anything again, and must leave a
conservative, correctly attributed result -- except when the journal itself cannot be written, which stops all new sending.

  * Shapes:   every wrong answer (array, null, empty object, wrong field type, scalar, short body, timeout) on every route in every phase
              (startup probe, before the send, after the send, at recovery, at reconcile).  F04 already closed these; this is the guard against
              their return, so it is expected to pass on the P0 code as well.
  * Finishing: a client that raises, an exception inside the exception handling, a record that is not what this bridge writes, a trace
              that cannot be written, many failures in a row -- and then a normal action in the same process.
  * Storage:  the journal cannot be synced: nothing is sent after it, the error is diagnosable, a restart finds a journal it can replay."""
import errno
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from p0_support import AID, Client, close, engine, envelope, payload, reg_for
from test_p1_recovery import REMOVE, cut_after, edit, jlines, kinds, rewrite
from bridge import durable, preflight
from bridge.journal import DAY_MS, Journal
try:
    from bridge.journal import JournalWriteError
except ImportError:                                    # the P0 bridge has none: the tests that need it then fail by assertion
    class JournalWriteError(Exception):
        pass

START = 1800000000000
AID2 = 'b' * 32


def raised(fn, *args):
    """The exception fn raises (any), or None."""
    try:
        fn(*args)
    except Exception as exc:  # noqa: BLE001
        return exc
    return None


def terminals(tmp, aid=AID):
    """The records that end an action: `terminal` once it was sent, `gate_reject` when it was refused before."""
    recs = [json.loads(l) for l in jlines(tmp)]
    return [r for r in recs if r['kind'] in ('terminal', 'gate_reject') and r.get('action_id') == aid]


def posts_for(client, aid):
    return [p for p in client.posts if p['origin'] and p['origin'].endswith(aid[:8])]


# ---- wrong answers ---------------------------------------------------------------------------------------------------------------
def bad_answer(path, shape, good):
    base = dict(method='GET', path=path, origin=None, req_body=None, t_send_ms=0, t_recv_ms=0)
    if shape in ('timeout', 'short body'):
        return None, dict(base, kind='error', error_kind='timeout' if shape == 'timeout' else 'truncated', error_text='')
    if shape == 'null':
        return None, dict(base, kind='response', status=200, content_type='application/json', text='null', json=None)
    body = {'array': [] if path != '/audit' else {}, 'empty object': {}, 'scalar': 5,
            'wrong field type': {'/status': dict(good, runtime_id=5), '/health': dict(good, status=5), '/audit': 'x'}[path]}[shape]
    return body, dict(base, kind='response', status=200, content_type='application/json', text=json.dumps(body), json=body)


SHAPES = ('array', 'null', 'empty object', 'wrong field type', 'scalar', 'short body', 'timeout')
ROUTES = ('/status', '/health', '/audit')


class ShapeClient(Client):
    """p0_support.Client, except that one route gives a wrong answer: from the start (`when` = 'always') or after the first POST."""

    def __init__(self, clock):
        super().__init__(clock)
        self.route = self.shape = None
        self.when = 'always'

    def get_json(self, path):
        body, rec = super().get_json(path)
        if path == self.route and (self.when == 'always' or self.posts):
            return bad_answer(path, self.shape, body)
        return body, rec


class Shapes(unittest.TestCase):
    def check_event(self, tmp, aid, status, posts, client, e):
        ev = e.journal.final_event(aid)
        self.assertEqual(ev['status'], status)
        self.assertNotEqual(ev['status'], 'verified')
        self.assertEqual(len(posts_for(client, aid)), posts, 'POSTs for the action')
        self.assertEqual(len(terminals(tmp, aid)), 1, 'one terminal record')
        self.assertEqual(e.counters['internal_errors'], 0, 'a wrong answer is an expected case, not an internal error')

    def test_F07_01_every_wrong_answer_on_every_route_in_every_phase(self):
        for route in ROUTES:
            for shape in SHAPES:
                for phase in ('pre', 'post', 'recovery', 'reconcile', 'startup'):
                    with self.subTest(route=route, shape=shape, phase=phase), tempfile.TemporaryDirectory() as tmp:
                        clock = [START]
                        client = ShapeClient(clock)
                        e = engine(tmp, clock, client=client)
                        line = envelope(payload(clock[0]))
                        if phase == 'startup':
                            client.route, client.shape = route, shape
                            probe = raised(preflight.check, client, reg_for())
                            self.assertNotIsInstance(probe, Exception, 'the startup probe raised %r' % (probe,))
                            result = preflight.check(client, reg_for())
                            self.assertEqual(result['reachable'], not (route == '/status' and shape in ('null', 'short body', 'timeout')))
                            if route != '/audit':
                                self.assertTrue(result['problems'] or not result['reachable'])
                            self.assertEqual(client.posts, [])
                        elif phase in ('pre', 'post'):
                            client.route, client.shape, client.when = route, shape, 'always' if phase == 'pre' else 'after_post'
                            exc = raised(e.safe_handle_line, line)
                            self.assertIsNone(exc, 'safe_handle_line raised %r' % (exc,))
                            want = ('accepted', 1) if route == '/audit' else (('rejected', 0) if phase == 'pre' else ('unknown', 1))
                            self.check_event(tmp, AID, *want, client, e)
                        elif phase == 'recovery':
                            class Crash(Exception):
                                pass

                            def stop():
                                raise Crash()
                            e.hooks = type('H', (), {'after_response': staticmethod(stop)})()
                            self.assertIsInstance(raised(e.handle_line, line), Crash)
                            close(e)
                            client.route, client.shape = route, shape
                            e = engine(tmp, clock, client=client)
                            exc = raised(e.recover)
                            self.assertIsNone(exc, 'recover raised %r' % (exc,))
                            self.check_event(tmp, AID, 'accepted' if route == '/audit' else 'unknown', 1, client, e)
                        else:                                                  # reconcile
                            client.bad_after_post = True                      # a live result that is `unknown` and so is looked up again
                            e.handle_line(line)
                            before = e.journal.final_event(AID)
                            self.assertEqual((before['status'], before['reason']), ('unknown', 'runtime_unreachable'))
                            client.bad_after_post = False
                            client.route, client.shape = route, shape
                            clock[0] += 5000
                            n = len(jlines(tmp))
                            exc = raised(e.reconcile_unknowns)
                            self.assertIsNone(exc, 'reconcile_unknowns raised %r' % (exc,))
                            self.assertEqual(e.journal.final_event(AID), before, 'a reconcile never changes the status of an action')
                            self.assertEqual(e.counters['internal_errors'], 0)
                            if route != '/health':
                                self.assertEqual(len(jlines(tmp)), n, 'a look that could not be made leaves nothing behind')
                        close(e)

    def test_F07_02_after_the_wrong_answers_the_same_process_serves_the_next_action(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = ShapeClient(clock)
            e = engine(tmp, clock, client=client)
            for i, (route, shape) in enumerate((r, s) for r in ('/status', '/health') for s in SHAPES):
                client.route, client.shape = route, shape
                aid = '%032x' % (i + 1)
                self.assertIsNone(raised(e.safe_handle_line, envelope(payload(clock[0], action_id=aid))))
                self.assertEqual(e.journal.final_event(aid)['status'], 'rejected')
            self.assertEqual(client.posts, [])
            client.route = None
            e.safe_handle_line(envelope(payload(clock[0], action_id=AID2)))
            self.assertEqual(e.journal.final_event(AID2)['status'], 'accepted')
            self.assertEqual(len(posts_for(client, AID2)), 1)
            self.assertEqual(len(client.posts), 1)
            close(e)


# ---- unexpected exceptions in the finishing path ----------------------------------------------------------------------------------
class Boom(Exception):
    pass


class ExplodingClient(Client):
    """Healthy, except that once armed (or once a POST has been made, if `after_post`) every GET raises Boom: the client itself fails, which is
    not a wrong answer."""

    def __init__(self, clock):
        super().__init__(clock)
        self.armed = self.after_post = self.boomed = False
        self.gets_after_boom = 0

    def get_json(self, path):
        if self.armed or (self.after_post and self.posts):
            self.gets_after_boom += self.boomed
            self.boomed = True
            raise Boom(path)
        return super().get_json(path)


class Finishing(unittest.TestCase):
    def test_F07_03_a_failure_in_the_post_check_and_again_in_the_recovery_ends_conservatively(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = ExplodingClient(clock)
            client.after_post = True
            e = engine(tmp, clock, client=client)
            line = envelope(payload(clock[0]))
            exc = raised(e.safe_handle_line, line)
            self.assertIsNone(exc, 'the exception handling raised %r' % (exc,))
            ev = e.journal.final_event(AID)
            self.assertIsNotNone(ev, 'the action was left without a result')
            self.assertEqual((ev['status'], ev['reason']), ('unknown', 'runtime_unreachable'))
            self.assertIn('journal alone', ev.get('detail', ''))
            self.assertEqual(len(posts_for(client, AID)), 1)
            self.assertLessEqual(client.gets_after_boom, 1, 'the last resort must not read the runtime again')
            self.assertGreaterEqual(e.counters['internal_errors'], 1)
            self.assertEqual(len(terminals(tmp)), 1)
            # the process is alive: heal the client and serve a new action, then repeat the old line and restart twice
            client.after_post = False
            e.safe_handle_line(envelope(payload(clock[0], action_id=AID2)))
            self.assertEqual(e.journal.final_event(AID2)['status'], 'accepted')
            e.safe_handle_line(line)
            close(e)
            for _ in range(2):
                e = engine(tmp, clock, client=client)
                e.recover()
                e.safe_handle_line(line)
                self.assertEqual(e.journal.final_event(AID), ev)
                close(e)
            self.assertEqual((len(posts_for(client, AID)), len(posts_for(client, AID2))), (1, 1), 'one POST per action, counted per action')

    def test_F07_04_recovery_at_startup_does_not_depend_on_the_runtime_being_readable(self):
        for elapsed in (0, DAY_MS + 1):
            for crash_after in ('intent', 'response', 'post_check'):
                with self.subTest(elapsed=elapsed, crash_after=crash_after), tempfile.TemporaryDirectory() as tmp:
                    clock = [START]
                    client = ExplodingClient(clock)
                    e = engine(tmp, clock, client=client)
                    line = envelope(payload(clock[0]))
                    e.handle_line(line)
                    close(e)
                    cut_after(tmp, crash_after)
                    clock[0] += elapsed
                    client.armed = True
                    e = engine(tmp, clock, client=client)
                    exc = raised(e.recover)
                    self.assertIsNone(exc, 'recover() raised %r: a service that crashes here crashes again at every restart' % (exc,))
                    ev = e.journal.final_event(AID)
                    self.assertIsNotNone(ev, 'the action was left without a result')
                    self.assertEqual((ev['status'], ev['reason']), ('unknown', 'runtime_unreachable'))
                    self.assertEqual(len(client.posts), 1)
                    close(e)
                    client.armed = False
                    e = engine(tmp, clock, client=client)
                    e.recover()
                    e.safe_handle_line(line)
                    self.assertEqual(e.journal.final_event(AID), ev, 'a terminal state is never upgraded by a healthier restart')
                    self.assertEqual(len(client.posts), 1)
                    close(e)

    def test_F07_05_a_record_that_is_not_what_this_bridge_writes_does_not_end_the_service(self):
        for name, fn in {'intent without info': edit('intent', info=REMOVE), 'intent info is text': edit('intent', info='x'),
                         'intent without bits': edit('intent', bits=REMOVE), 'intent without label': edit('intent', label=REMOVE)}.items():
            with self.subTest(record=name), tempfile.TemporaryDirectory() as tmp:
                clock = [START]
                client = ExplodingClient(clock)
                e = engine(tmp, clock, client=client)
                e.handle_line(envelope(payload(clock[0])))
                close(e)
                cut_after(tmp, 'intent')
                if 'label' in name:
                    # the label is what the journal indexes: keep the file replayable by giving it another one
                    rewrite(tmp, edit('intent', label='tier0.x.00000000'))
                else:
                    rewrite(tmp, fn)
                e = engine(tmp, clock, client=client)
                exc = raised(e.recover)
                self.assertIsNone(exc, 'recover() raised %r' % (exc,))
                ev = e.journal.final_event(AID)
                self.assertIsNotNone(ev)
                self.assertEqual(ev['status'], 'unknown')
                self.assertEqual(len(client.posts), 1)
                close(e)

    def test_F07_06_many_failures_in_a_row_each_recorded_once_and_none_ends_the_process(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = ExplodingClient(clock)
            client.armed = True
            e = engine(tmp, clock, client=client)
            lines = [envelope(payload(clock[0], action_id='%032x' % (i + 1))) for i in range(8)]
            for line in lines:
                self.assertIsNone(raised(e.safe_handle_line, line))
            self.assertEqual(e.counters['internal_errors'], 8)
            self.assertEqual(kinds(tmp).count('internal_error'), 8)
            for line in lines:                                          # the same lines again: quarantined, nothing more is written
                self.assertIsNone(raised(e.safe_handle_line, line))
            self.assertEqual(kinds(tmp).count('internal_error'), 8)
            client.armed = False
            e.safe_handle_line(envelope(payload(clock[0], action_id=AID2)))
            self.assertEqual(e.journal.final_event(AID2)['status'], 'accepted')
            self.assertEqual(len(client.posts), 1)
            close(e)

    def test_F07_15_a_reconcile_look_that_raises_leaves_the_status_alone_and_is_not_repeated(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = ExplodingClient(clock)
            client.bad_after_post = True                               # the live result is `unknown`, which gets looked up again
            e = engine(tmp, clock, client=client)
            e.handle_line(envelope(payload(clock[0])))
            before = e.journal.final_event(AID)
            self.assertEqual((before['status'], before['reason']), ('unknown', 'runtime_unreachable'))
            client.bad_after_post = False
            client.armed = True
            clock[0] += 5000
            self.assertIsNone(raised(e.reconcile_unknowns), 'one action\'s look must not end the service')
            self.assertEqual(e.journal.final_event(AID), before)
            self.assertEqual(e.counters['internal_errors'], 1)
            calls = client.gets_after_boom
            self.assertIsNone(raised(e.reconcile_unknowns))
            self.assertEqual((client.gets_after_boom, e.counters['internal_errors']), (calls, 1), 'a look that failed is not made again in this process')
            client.armed = False
            close(e)
            e = engine(tmp, clock, client=client)                      # the next start looks again
            e.reconcile_unknowns()
            self.assertEqual(e.journal.final_event(AID), before)
            self.assertIn('reconcile', kinds(tmp))
            close(e)

    def test_F07_07_a_trace_that_cannot_be_written_does_not_lose_the_result_and_the_line_is_not_sent_again(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = Client(clock)
            e = engine(tmp, clock, client=client)
            line = envelope(payload(clock[0]))
            with mock.patch.object(e.trace, 'emit', side_effect=OSError(errno.ENOSPC, 'injected: no space left')):
                exc = raised(e.safe_handle_line, line)
            self.assertIsNone(exc)
            self.assertEqual(e.journal.final_event(AID)['status'], 'accepted', 'the terminal is in the journal, which is the primary record')
            self.assertEqual(len(client.posts), 1)
            self.assertEqual(e.counters['internal_errors'], 1)
            e.handle_line(line)                                       # the repeat re-emits the recorded event now that the trace works
            lines = [json.loads(l) for l in (Path(tmp) / 'trace.jsonl').read_text().splitlines()]
            self.assertEqual([l['status'] for l in lines if l.get('action_id') == AID], ['accepted'])
            self.assertEqual(len(client.posts), 1)
            close(e)


# ---- the journal cannot be written -------------------------------------------------------------------------------------------------
class Storage(unittest.TestCase):
    def sync_fails(self, e, state):
        real, fd = durable.sync, e.journal._fh.fileno()

        def sync(fileno):
            if state['fail'] and fileno == fd:
                raise OSError(errno.EIO, 'injected: the disk went away')
            return real(fileno)
        return mock.patch.object(durable, 'sync', side_effect=sync)

    def run_case(self, trip_at):
        """One request on a journal whose sync starts failing at `trip_at` ('intent' | 'response' | 'terminal'); returns what the tests look at."""
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        clock = [START]
        client = Client(clock)
        e = engine(tmp.name, clock, client=client)
        state = {'fail': trip_at == 'intent'}
        hook = {'response': 'after_http', 'terminal': 'after_post_check'}.get(trip_at)
        if hook:
            e.hooks = type('H', (), {hook: staticmethod(lambda: state.update(fail=True))})()
        line = envelope(payload(clock[0]))
        with self.sync_fails(e, state):
            exc = raised(e.safe_handle_line, line)
            self.assertIsInstance(exc, JournalWriteError, 'a journal that cannot be synced must stop the bridge with a diagnosable error, got %r' % (exc,))
            self.assertIn('journal', str(exc))
            posts_then, gets_then = len(client.posts), client.n
            second = raised(e.safe_handle_line, envelope(payload(clock[0], action_id=AID2)))
            self.assertIsInstance(second, JournalWriteError, 'after the failure nothing more is handled')
            self.assertEqual((len(client.posts), client.n), (posts_then, gets_then), 'no POST and not even a read for the next request')
            self.assertTrue(e.journal.failed)
            size = len(Path(tmp.name, 'journal.jsonl').read_bytes())
            self.assertIsInstance(raised(e.journal.append, 'start'), JournalWriteError)
            self.assertEqual(len(Path(tmp.name, 'journal.jsonl').read_bytes()), size, 'a failed journal writes nothing more')
        self.assertIsNone(raised(e.journal.close), 'closing a failed journal must not raise')
        e.trace.close()
        return tmp.name, clock, client, line

    def test_F07_08_the_sync_fails_before_the_send_nothing_is_sent_ever(self):
        tmp, clock, client, line = self.run_case('intent')
        self.assertEqual(client.posts, [])
        e = engine(tmp, clock, client=client)                         # the next start: the journal replays, whatever was cut or kept
        e.recover()
        e.safe_handle_line(line)
        self.assertLessEqual(len(client.posts), 1)
        self.assertEqual(len(terminals(tmp)), 1)
        close(e)

    def test_F07_09_the_sync_fails_after_the_answer_one_post_is_all_there_will_ever_be(self):
        tmp, clock, client, line = self.run_case('response')
        self.assertEqual(len(posts_for(client, AID)), 1)
        for _ in range(2):
            e = engine(tmp, clock, client=client)
            e.recover()
            e.safe_handle_line(line)
            self.assertIn(e.journal.final_event(AID)['status'], ('unknown', 'accepted'))
            close(e)
        self.assertEqual(len(posts_for(client, AID)), 1)
        self.assertEqual(len(terminals(tmp)), 1)

    def test_F07_10_the_sync_fails_at_the_terminal_the_restart_concludes_from_what_is_on_disk(self):
        tmp, clock, client, line = self.run_case('terminal')
        self.assertEqual(len(posts_for(client, AID)), 1)
        e = engine(tmp, clock, client=client)
        e.recover()
        ev = e.journal.final_event(AID)
        self.assertEqual(ev['status'], 'accepted', 'response and healthy post-check are on disk')
        self.assertEqual(len(posts_for(client, AID)), 1)
        self.assertEqual(len(terminals(tmp)), 1)
        close(e)

    def test_F07_11_a_failed_journal_beats_a_failing_client_in_the_handler(self):
        with tempfile.TemporaryDirectory() as tmp:
            clock = [START]
            client = ExplodingClient(clock)
            client.after_post = True
            e = engine(tmp, clock, client=client)
            state = {'fail': False}
            e.hooks = type('H', (), {'after_http': staticmethod(lambda: state.update(fail=True))})()
            with self.sync_fails(e, state):
                exc = raised(e.safe_handle_line, envelope(payload(clock[0])))
            self.assertIsInstance(exc, JournalWriteError)
            self.assertEqual(len(client.posts), 1)
            e.journal.close()
            e.trace.close()


if __name__ == '__main__':
    unittest.main()
