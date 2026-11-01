"""Shared by tests/test_robustness.py and review/author-probes/fuzz_engine.py: odd-but-validly-signed payloads, a fake healthy
IA2 client, and a runner that returns what happened to each input."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path
import json
import os
import sys
import tempfile
import traceback

sys.dont_write_bytecode = True
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
from bridge import auth, registry  # noqa: E402
from bridge.engine import Engine  # noqa: E402
from bridge.journal import Journal  # noqa: E402
from signing_fixtures import TraceWriter  # noqa: E402

GOLDEN = os.path.join(ROOT, 'tests', 'fixtures', 'golden')


def golden(name):
    with open(os.path.join(GOLDEN, name + '.json'), encoding='utf-8') as f:
        return json.load(f)['body']


class FakeClient:
    """Looks like a healthy runtime whose scan_count advances at every read; /write echoes the request."""

    def __init__(self):
        self.n = 1000
        self.status, self.health = golden('A0-status'), golden('A0-health')
        self.audit = []
        self.calls = []

    def _rec(self, path, obj):
        text = json.dumps(obj)
        return {'kind': 'response', 'status': 200, 'text': text, 'json': obj, 't_send_ms': 0, 't_recv_ms': 0, 'path': path, 'method': 'GET'}

    def get_json(self, path):
        self.n += 50
        if path == '/status':
            obj = dict(self.status, scan_count=self.n)
        elif path == '/health':
            obj = dict(self.health, scan_count=self.n)
        else:
            obj = self.audit
        self.calls.append(path)
        return obj, self._rec(path, obj)

    def request(self, method, path, body=None, origin=None):
        obj = {'ok': True, 'name': body['name'], 'value': body['value'], 'undelivered_device': None}
        return dict(self._rec(path, obj), method=method, req_body=json.dumps(body), origin=origin)


def payload(**over):
    p = {'event_id': 'e', 'action_id': os.urandom(16).hex(), 'session_id': 's', 'device_id': 'bench01-sim', 'command': 'ia2_set',
         'target_key': 'flow', 'target': 12.5, 'simulation': True, 'requested_at_ms': 0, 'deadline_ms': 0, 'requested_by': 'agent:fuzz'}
    p.update(over)
    return p


def run_all(handler='handle_line', verbose=True):
    reg_doc = {'version': 1, 'targets': [{'key': 'flow', 'device_id': 'bench01-sim', 'ia2_variable': 'sp_flow', 'type': 'REAL', 'min': 0.0,
                                          'max': 40.0, 'simulation': True, 'enabled': True, 'require_devices': ['coupler'],
                                          'min_interval_ms': 0, 'max_rate_per_min': 60}]}
    reg = registry.parse(json.dumps(reg_doc), 'x')
    keys = {'k1': {'secret': b'\x07' * 32, 'not_before_ms': 0, 'not_after_ms': 2 ** 60}}
    cases = []
    big = 10 ** 400
    for label, over in [
        ('target = 10**400', {'target': big}), ('target = -10**400', {'target': -big}), ('target = 1e308', {'target': 1e308}),
        ('target = 5e-324', {'target': 5e-324}), ('target = 1e-45 (below f32 normal)', {'target': 1e-45}),
        ('requested_at_ms = 10**30', {'requested_at_ms': 10 ** 30}), ('deadline_ms = 10**30', {'deadline_ms': 10 ** 30}),
        ('deadline_ms = -10**30', {'deadline_ms': -10 ** 30}),
        ('target_key = lone surrogate (JSON \\ud800)', {'target_key': '\ud800'}), ('session_id = lone surrogate', {'session_id': '\udfff'}),
        ('device_id = lone surrogate', {'device_id': '\ud800x'}), ('requested_by = lone surrogate', {'requested_by': '\ud800'}),
        ('target_key = NUL bytes', {'target_key': 'a\x00b'}), ('target_key = 300 characters', {'target_key': 'k' * 300}),
        ('requested_by = non-ASCII', {'requested_by': 'agent:温度'}), ('requested_by = ""', {'requested_by': ''}),
        ('target = nested list', {'target': [1, [2]]}), ('target = nested dict', {'target': {'a': 1}}),
        ('action_id = uppercase hex', {'action_id': 'A' * 32}), ('event_id = 100 KB', {'event_id': 'x' * 100000}),
    ]:
        cases.append((label, json.dumps(payload(**over))))
    deep = '[' * 200000 + ']' * 200000
    cases.append(('payload nested 200000 deep', deep))
    cases.append(('payload is a JSON string', json.dumps('hello')))
    cases.append(('payload is NaN literal', '{"target": NaN}'))
    cases.append(('payload is empty', ''))
    cases.append(('payload with a lone-surrogate escape in a key', '{"\\ud800": 1}'))

    tmp = tempfile.mkdtemp()
    problems = []
    for label, text in cases:
        jr = Journal(os.path.join(tmp, 'j-%d.jsonl' % len(problems)), lambda: 1000)
        jr.open()
        t_ms = [1_700_000_000_000]

        def clock():
            t_ms[0] += 1
            return t_ms[0]
        engine = Engine({'poll_ms': 1, 'advance_wait_ms': 50, 'hold_path': None, 'project_dir': None}, reg, keys, jr, FakeClient(),
                        TraceWriter(os.path.join(tmp, 't-%d.jsonl' % len(problems))), clock=clock, sleep=lambda s: None)
        try:
            p = json.loads(text) if text.startswith('{') and 'NaN' not in text and 'ud800' not in text else None
        except ValueError:
            p = None
        if p is not None and 'requested_at_ms' in p and p['requested_at_ms'] == 0:
            p['requested_at_ms'], p['deadline_ms'] = t_ms[0], t_ms[0] + 5000
            text = json.dumps(p)
        if p is not None and p.get('deadline_ms') == 0:
            p['deadline_ms'] = t_ms[0] + 5000
            text = json.dumps(p)
        try:
            line = json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': text, 'sig': auth.sign(keys['k1']['secret'], text)})
        except UnicodeEncodeError:
            line = json.dumps({'v': 'ia2.control/v0', 'key_id': 'k1', 'payload': text, 'sig': '0' * 64})
        try:
            getattr(engine, handler)(line)
            if verbose:
                print('ok      %s' % label)
        except Exception as exc:  # noqa: BLE001
            tb = traceback.extract_tb(exc.__traceback__)[-1]
            problems.append(label)
            if verbose:
                print('RAISED  %s -> %s: %s   (%s:%d)' % (label, type(exc).__name__, str(exc)[:70], os.path.relpath(tb.filename, ROOT), tb.lineno))
        jr.close()
        engine.trace.close()
    return problems, len(cases)




def read_journal(path):
    with open(path, encoding='ascii') as f:
        return [json.loads(l) for l in f if l.strip()]
