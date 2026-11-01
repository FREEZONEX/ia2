"""Local stdio MCP: two registered control tools, signed file transport only.

No listener, arbitrary runtime URL, variable name, raw bits or force surface.
The bridge alone submits runtime writes. This server is simulation-only.
"""
import argparse
import fcntl
import hashlib
import json
import os
import re
import secrets
import sys
import time

from bridge import auth as request_auth, registry, values
from bridge.errors import Reject
from readside import auth, verifier

PROTOCOL = '2025-11-25'
MAX_FRAME = 65536
MAX_RECORDS = 20000


class EvidenceIndex:
    """Authenticate only new complete lines; bounded cache, fail closed on overflow.

Replacement/truncation rebuilds the index. There is no silently incomplete
history: exceeding the local example's budget is a tool error.
"""
    def __init__(self, path, keys, role, revoked):
        self.path, self.keys, self.role, self.revoked = path, keys, role, revoked
        self.identity, self.offset, self.rows, self.rejected = None, 0, [], 0

    def refresh(self):
        try:
            f = open(self.path, 'rb')
        except FileNotFoundError:
            self.identity, self.offset, self.rows, self.rejected = None, 0, [], 0
            return
        with f:
            st = os.fstat(f.fileno())
            ident = (st.st_dev, st.st_ino)
            if ident != self.identity or st.st_size < self.offset:
                self.offset, self.rows, self.rejected = 0, [], 0
            self.identity = ident
            f.seek(self.offset)
            while True:
                line = f.readline(auth.MAX_ENVELOPE + 1)
                if not line:
                    break
                if len(line) > auth.MAX_ENVELOPE:
                    raise ValueError('evidence line exceeds local size limit')
                if not line.endswith(b'\n'):
                    break  # writer is still appending; retry from the same offset
                if len(self.rows) + self.rejected >= MAX_RECORDS:
                    raise ValueError('evidence history exceeds local record limit')
                self.offset = f.tell()
                try:
                    record = auth.verify(line, self.keys, self.role, self.revoked)
                except ValueError:
                    self.rejected += 1
                    continue
                self.rows.append((record, line))


class ControlTools:
    def __init__(self, args):
        self.args = args
        self.registry = registry.load_pinned(args.registry, args.registry_sha256)
        if any(e['enabled'] and not e['simulation'] for e in self.registry.targets.values()):
            raise ValueError('local MCP requires simulation targets')
        self.keys = auth.load_keys(args.keys)
        self.event_keys = auth.load_keys(args.event_keys)
        self.sample_keys = auth.load_keys(args.sample_keys)
        auth.separate_keys(self.keys, self.event_keys, self.sample_keys)
        self.revoked = auth.load_revoked(args.revoked_keys)
        self.trace = EvidenceIndex(args.trace, self.event_keys, 'event', self.revoked)
        self.samples = EvidenceIndex(args.samples, self.sample_keys, 'sample', self.revoked)
        self.requests = {}
        self._request_identity, self._request_offset = None, 0
        self._request_lines = 0
        self.session = secrets.token_hex(16)

    def definitions(self):
        targets = sorted(k for k, e in self.registry.targets.items() if e['enabled'])
        return [
            {'name': 'ia2_set', 'description': 'Submit a signed simulation setpoint request. The returned ID is not execution success.',
             'inputSchema': {'type': 'object', 'properties': {
                 'target_key': {'type': 'string', 'enum': targets}, 'target': {'type': ['number', 'boolean']},
                 'ttl_ms': {'type': 'integer', 'minimum': 1, 'default': 20000}},
                 'required': ['target_key', 'target'], 'additionalProperties': False},
             'annotations': {'readOnlyHint': False, 'destructiveHint': True, 'idempotentHint': False, 'openWorldHint': False}},
            {'name': 'get_action_status', 'description': 'Derive an action status from authenticated bridge events and independent samples.',
             'inputSchema': {'type': 'object', 'properties': {'action_id': {'type': 'string', 'pattern': '^[0-9a-f]{32}$'}},
                             'required': ['action_id'], 'additionalProperties': False},
             'annotations': {'readOnlyHint': True, 'idempotentHint': True, 'openWorldHint': False}}]

    def _load_requests(self):
        try:
            f = open(self.args.requests, 'rb')
        except FileNotFoundError:
            self.requests, self._request_identity, self._request_offset = {}, None, 0
            self._request_lines = 0
            return
        with f:
            st = os.fstat(f.fileno())
            identity = st.st_dev, st.st_ino
            if identity != self._request_identity or st.st_size < self._request_offset:
                self.requests, self._request_offset = {}, 0
                self._request_lines = 0
            self._request_identity = identity
            f.seek(self._request_offset)
            while True:
                line = f.readline(MAX_FRAME + 1)
                if not line:
                    break
                if len(line) > MAX_FRAME:
                    raise ValueError('request line exceeds local size limit')
                if not line.endswith(b'\n'):
                    break
                if self._request_lines >= MAX_RECORDS:
                    raise ValueError('request history exceeds local record limit')
                self._request_lines += 1
                self._request_offset = f.tell()
                try:
                    env = auth.loads(line)
                    kid = env.get('key_id')
                    if kid in self.revoked or kid not in self.keys:
                        continue
                    # Verify the MAC before parsing. Historical requests are
                    # checked against their signed creation time, not now.
                    text, _ = request_auth.verify_envelope(line, self.keys, self.keys[kid]['not_before_ms'])
                    p = auth.loads(text)
                    t = p['requested_at_ms']
                    if type(t) is not int or not self.keys[kid]['not_before_ms'] <= t <= self.keys[kid]['not_after_ms']:
                        continue
                    aid = p['action_id']
                    if not isinstance(aid, str) or not re.fullmatch('[0-9a-f]{32}', aid):
                        continue
                    if aid in self.requests and self.requests[aid] != p:
                        self.requests[aid] = None  # conflicting authenticated request is not a usable target binding
                    else:
                        self.requests[aid] = p
                except (ValueError, TypeError, KeyError, AttributeError, Reject):
                    continue
                if len(self.requests) > MAX_RECORDS:
                    raise ValueError('request history exceeds local record limit')

    def ia2_set(self, target_key, target, ttl_ms=20000):
        if not isinstance(target_key, str):
            raise ValueError('target_key must be a registered string')
        entry = self.registry.targets.get(target_key)
        if entry is None or not entry['enabled']:
            raise ValueError('target is not enabled in the pinned registry')
        if type(ttl_ms) is not int or not 1 <= ttl_ms <= entry['max_deadline_ms']:
            raise ValueError('ttl_ms is outside the registered deadline budget')
        values.validate_and_encode(entry, target)
        now = int(time.time() * 1000)
        kid, key = auth.select_key(self.keys, now, self.revoked)
        p = dict(event_id=secrets.token_hex(16), action_id=secrets.token_hex(16), session_id=self.session,
                 device_id=entry['device_id'], command='ia2_set', target_key=target_key, target=target,
                 simulation=True, requested_at_ms=now, deadline_ms=now + ttl_ms, requested_by='agent:local-mcp')
        payload = json.dumps(p, sort_keys=True, separators=(',', ':'), allow_nan=False)
        env = dict(v='ia2.control/v0', key_id=kid, payload=payload, sig=request_auth.sign(key['secret'], payload))
        data = (json.dumps(env, separators=(',', ':')) + '\n').encode('ascii')
        fd = os.open(self.args.requests, os.O_RDWR | os.O_APPEND | os.O_CREAT, 0o600)
        with os.fdopen(fd, 'a+b', buffering=0) as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            f.seek(0, os.SEEK_END)
            offset = f.tell()
            if offset:
                f.seek(-1, os.SEEK_END)
                if f.read(1) != b'\n':
                    raise ValueError('request log has an incomplete tail; recover it before submitting')
            # A failure after bytes are visible is ambiguous: never auto-retry.
            try:
                remaining = memoryview(data)
                while remaining:
                    count = f.write(remaining)
                    if not count:
                        raise OSError('request write made no progress')
                    remaining = remaining[count:]
                os.fsync(f.fileno())
            except OSError:
                return dict(action_id=p['action_id'], status='unknown', reason='request_persistence_unconfirmed',
                            detail='Do not repeat the setpoint automatically; query this action ID.')
        return dict(action_id=p['action_id'], status='requested')

    def get_action_status(self, action_id):
        if not isinstance(action_id, str) or not re.fullmatch('[0-9a-f]{32}', action_id):
            raise ValueError('invalid action_id')
        self._load_requests()
        request = self.requests.get(action_id)
        if not request:
            raise ValueError('unknown or conflicting authenticated action')
        entry = self.registry.targets.get(request.get('target_key'))
        if entry is None or request.get('device_id') != entry['device_id'] or request.get('simulation') is not True:
            raise ValueError('action does not match the pinned registry')
        self.trace.refresh()
        self.samples.refresh()
        events = [(r, raw) for r, raw in self.trace.rows if r.get('action_id') == action_id]
        runtime_ids = {r.get('runtime_id') for r, _ in events}
        # Authenticate before using identity/timestamps to select candidates.
        start, end = request['requested_at_ms'] - 1000, request['deadline_ms'] + 2000
        samples = [raw for r, raw in self.samples.rows if start <= r['collected_at_ms'] <= end
                   or (r['native_runtime_id'] in runtime_ids and start <= r.get('observed_at_ms', -1) <= end)]
        result = verifier.derive(action_id, entry, [raw for _, raw in events], samples, int(time.time() * 1000),
                                 registry_sha256=self.registry.sha256, event_keys=self.event_keys,
                                 sample_keys=self.sample_keys, revoked=self.revoked)
        result['rejected_envelopes'] += self.trace.rejected + self.samples.rejected
        result['rejected_by_role']['event'] += self.trace.rejected
        result['rejected_by_role']['sample'] += self.samples.rejected
        return result


class Server:
    def __init__(self, tools):
        self.tools, self.initialized, self.ready = tools, False, False

    def dispatch(self, message):
        if not isinstance(message, dict) or message.get('jsonrpc') != '2.0' or not isinstance(message.get('method'), str):
            return self.error(None, -32600, 'Invalid Request')
        has_id, ident = 'id' in message, message.get('id')
        if has_id and (type(ident) not in (int, str)):
            return self.error(None, -32600, 'Invalid Request')
        method, params = message['method'], message.get('params', {})
        if not has_id:
            if method == 'notifications/initialized' and self.initialized:
                self.ready = True
            return None
        if not isinstance(params, dict):
            return self.error(ident, -32602, 'Invalid params')
        if method == 'initialize':
            if self.initialized or not isinstance(params.get('protocolVersion'), str) \
                    or not isinstance(params.get('capabilities'), dict) or not isinstance(params.get('clientInfo'), dict):
                return self.error(ident, -32602, 'Invalid initialize params or already initialized')
            self.initialized = True
            result = dict(protocolVersion=PROTOCOL, capabilities={'tools': {'listChanged': False}},
                          serverInfo={'name': 'ia2-local-control', 'version': '0.1.0'},
                          instructions='Simulation only. A submitted request is not proof of execution or physical effect.')
        elif method == 'ping':
            result = {}
        elif not self.ready:
            return self.error(ident, -32002, 'Initialize and send notifications/initialized first')
        elif method == 'tools/list':
            if params.get('cursor') is not None:
                return self.error(ident, -32602, 'No pagination cursor is supported')
            result = {'tools': self.tools.definitions()}
        elif method == 'tools/call':
            name, args = params.get('name'), params.get('arguments', {})
            allowed = {'ia2_set': ({'target_key', 'target', 'ttl_ms'}, {'target_key', 'target'}),
                       'get_action_status': ({'action_id'}, {'action_id'})}
            if not isinstance(name, str) or name not in allowed:
                return self.error(ident, -32602, 'Unknown tool')
            fields, required = allowed[name]
            if not isinstance(args, dict) or not required <= set(args) <= fields:
                return self.error(ident, -32602, 'Invalid tool arguments')
            try:
                value = getattr(self.tools, name)(**args)
                result = {'content': [{'type': 'text', 'text': json.dumps(value, ensure_ascii=True)}], 'isError': False}
            except (ValueError, OSError, Reject, registry.RegistryError) as exc:
                code = exc.reason if isinstance(exc, Reject) else ('local_io_error' if isinstance(exc, OSError) else 'tool_refused')
                message = 'Local files are unavailable; no automatic retry.' if isinstance(exc, OSError) else str(exc)
                result = {'content': [{'type': 'text', 'text': json.dumps({'error': {'code': code, 'message': message}})}], 'isError': True}
            except (KeyError, TypeError, AttributeError, OverflowError):
                # Authentication is not a schema guarantee. A signed producer
                # can still emit malformed data; keep the protocol alive and
                # never turn an interrupted operation into a retry or success.
                result = {'content': [{'type': 'text', 'text': json.dumps({'error': {
                    'code': 'local_data_error', 'message': 'Malformed local data; no automatic retry.'}})}], 'isError': True}
        else:
            return self.error(ident, -32601, 'Method not found')
        return dict(jsonrpc='2.0', id=ident, result=result)

    @staticmethod
    def error(ident, code, message):
        return dict(jsonrpc='2.0', id=ident, error=dict(code=code, message=message))

    def run(self, incoming, outgoing):
        while True:
            line = incoming.readline(MAX_FRAME + 1)
            if not line:
                return 0
            if len(line) > MAX_FRAME or not line.endswith(b'\n'):
                return 2  # framing lost; never process fragments as fresh requests
            try:
                message = auth.loads(line)
            except ValueError:
                response = self.error(None, -32700, 'Parse error')
            else:
                response = self.dispatch(message)
            if response is not None:
                outgoing.write(json.dumps(response, ensure_ascii=True, allow_nan=False).encode('ascii') + b'\n')
                outgoing.flush()


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__)
    for name in ('registry', 'registry-sha256', 'keys', 'event-keys', 'sample-keys', 'requests', 'trace', 'samples'):
        ap.add_argument('--' + name, required=True)
    ap.add_argument('--revoked-keys', help='JSON key-ID array; reloaded on server restart')
    args = ap.parse_args(argv)
    try:
        tools = ControlTools(args)
    except (ValueError, OSError, registry.RegistryError):
        print('MCP startup refused: check pinned simulation registry and independent mode-0600 keys', file=sys.stderr)
        return 2
    return Server(tools).run(sys.stdin.buffer, sys.stdout.buffer)
