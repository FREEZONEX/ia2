"""Local in-memory IA2 fixture for P0 regression tests; no external connections."""
import sys
sys.path.insert(0, __import__('os').path.dirname(__file__))
from signing_fixtures import event_keys_path
import copy
import hashlib
import json
import pathlib
import sys
import tempfile

sys.dont_write_bytecode = True
SRC = pathlib.Path(__file__).resolve().parents[1]
ROOT = SRC.parent
sys.path[:0] = [str(SRC), str(SRC / 'lab')]
from bridge import auth, registry, northbound_guard
from bridge.engine import Engine
from bridge.journal import Journal, DAY_MS
from signing_fixtures import TraceWriter

KEYS = {'k': {'secret': b'independent-review-fixture-key-32', 'not_before_ms': 0, 'not_after_ms': 2**60}}
AID = '0123456789abcdef0123456789abcdef'


def reg_for(**overrides):
    row = dict(key='set', device_id='d', ia2_variable='sp', type='DINT', min=0, max=100,
               enabled=True, simulation=True, require_devices=['dev'], min_interval_ms=0,
               max_rate_per_min=60, single_flight_ms=0)
    row.update(overrides)
    text = json.dumps({'version': 1, 'targets': [row]})
    return registry.parse(text, hashlib.sha256(text.encode()).hexdigest())


def payload(now, **overrides):
    p = dict(event_id='evt', action_id=AID, session_id='session', device_id='d', command='ia2_set',
             target_key='set', target=1, simulation=True, requested_at_ms=now, deadline_ms=now+5000,
             requested_by='agent:review')
    p.update(overrides)
    return p


def envelope(p):
    text = json.dumps(p)
    return json.dumps(dict(v='ia2.control/v0', key_id='k', payload=text, sig=auth.sign(KEYS['k']['secret'], text)))


class Client:
    def __init__(self, clock):
        self.clock, self.n, self.posts = clock, 0, []
        self.bad_after_post = False
        self.thin_after_post = False
        self.omit_forces = False
        self.read_delay_ms = 0

    def get_json(self, path):
        self.clock[0] += self.read_delay_ms
        self.n += 1
        if path == '/status':
            body = dict(runtime_id='R', scan_count=self.n, mode={'kind': 'running'}, forces=[],
                        fault=None, latch_reason=None, watchdog_tripped=False,
                        device_health=[dict(name='dev', healthy=True)],
                        last_snapshot=dict(timestamp_us=self.n*1000, vars=[]))
            if self.posts and self.bad_after_post:
                body = []
            elif self.posts and self.thin_after_post:
                body = dict(runtime_id='R', last_snapshot=dict(timestamp_us=self.n*1000))
            elif self.omit_forces:
                del body['forces']
        elif path == '/health':
            body = dict(status='ok', fault=None, watchdog_tripped=False, latch_reason=None,
                        fieldbus_healthy=True, snapshot_tap={'running': True})
            if self.posts and self.thin_after_post:
                body = {}
        else:
            body = []
        return body, dict(text=json.dumps(body), t_send_ms=self.clock[0], t_recv_ms=self.clock[0])

    def request(self, method, path, body=None, origin=None):
        self.posts.append(dict(method=method, path=path, body=body, origin=origin, at_ms=self.clock[0]))
        result = dict(ok=True, name=body['name'], value=body['value'], undelivered_device=None)
        return dict(kind='response', status=200, json=result, text=json.dumps(result),
                    t_send_ms=self.clock[0], t_recv_ms=self.clock[0])


def engine(directory, clock, reg=None, client=None):
    directory = pathlib.Path(directory)
    directory.mkdir(exist_ok=True, parents=True)
    jr = Journal(str(directory/'journal.jsonl'), lambda: clock[0])
    jr.open()
    tr = TraceWriter(str(directory/'trace.jsonl'))
    eng = Engine(dict(poll_ms=0, advance_wait_ms=600, hold_path=None, project_dir=None),
                 reg or reg_for(), KEYS, jr, client or Client(clock), tr,
                 clock=lambda: clock[0], sleep=lambda _: None)
    return eng


def close(eng):
    eng.journal.close()
    eng.trace.close()
