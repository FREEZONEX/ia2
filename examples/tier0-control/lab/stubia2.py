"""A scriptable stand-in for the IA2 edge runtime's HTTP API (B11).  /status, /health and /audit answer like a healthy
runtime (shapes copied from the WP0 golden samples, scan_count advancing every 100 ms); POST /write plays back a queue of
behaviours, so the bridge can be shown a 500, a non-JSON body, a body without `undelivered_device`, a redirect, a truncated
body, silence ...  Every request is logged.  Loopback only."""
import asyncio
import json
import time

REASONS = {200: 'OK', 302: 'Found', 403: 'Forbidden', 404: 'Not Found', 409: 'Conflict', 500: 'Internal Server Error'}


def http_response(status, body, ctype='application/json', extra=None, length=None):
    raw = body.encode() if isinstance(body, str) else body
    head = 'HTTP/1.1 %d %s\r\nContent-Type: %s\r\nContent-Length: %d\r\nConnection: close\r\n' % (
        status, REASONS.get(status, 'X'), ctype, len(raw) if length is None else length)
    for k, v in (extra or {}).items():
        head += '%s: %s\r\n' % (k, v)
    return head.encode() + b'\r\n' + raw


class StubIA2:
    def __init__(self, port, log_path):
        self.port = port
        self.log = open(log_path, 'w', buffering=1)
        self.behaviors = []
        self.server = None
        self.requests = []

    def script(self, *behaviors):
        self.behaviors.extend(behaviors)

    def _status(self):
        n = int(time.time() * 10)
        return {'runtime_id': 'STUB-1', 'scan_count': n, 'mode': {'kind': 'running'}, 'forces': [], 'fault': None,
                'latch_reason': None, 'watchdog_tripped': False, 'devices': ['coupler'],
                'device_health': [{'name': 'coupler', 'healthy': True}],
                'last_snapshot': {'scan_count': n, 'timestamp_us': n * 100000, 'device_health': [], 'vars': [
                    {'name': 'sp_flow', 'type_name': 'REAL', 'value': '0', 'bits': 0},
                    {'name': 'sp_flow_fb', 'type_name': 'REAL', 'value': '0', 'bits': 0,
                     'input': {'device': 'coupler', 'channel': 'sp_echo', 'stale': False}}]},
                'snapshot_tap': {'faults': 0, 'last_fault': None, 'last_fault_age_ms': None, 'running': True, 'stalled_for_ms': None}}

    def _health(self):
        return {'status': 'ok', 'fault': None, 'latch_reason': None, 'watchdog_tripped': False, 'fieldbus_healthy': True,
                'devices': [{'name': 'coupler', 'healthy': True}], 'scan_count': int(time.time() * 10), 'uptime_secs': 1,
                'snapshot_tap': {'faults': 0, 'last_fault': None, 'last_fault_age_ms': None, 'running': True, 'stalled_for_ms': None}}

    async def start(self):
        self.server = await asyncio.start_server(self._on_connect, '127.0.0.1', self.port, reuse_address=True)

    async def stop(self):
        if self.server:
            self.server.close()
            await self.server.wait_closed()
        self.log.close()

    async def _on_connect(self, reader, writer):
        try:
            head = await asyncio.wait_for(reader.readuntil(b'\r\n\r\n'), 5)
            lines = head.decode('latin1').split('\r\n')
            method, path = lines[0].split(' ')[:2]
            headers = {k.strip().lower(): v.strip() for k, _, v in (l.partition(':') for l in lines[1:] if ':' in l)}
            body = (await reader.readexactly(int(headers.get('content-length', '0')))).decode('utf-8', 'replace')
            rec = {'t': round(time.time(), 6), 'method': method, 'path': path, 'origin': headers.get('x-ia2-origin'), 'body': body}
            self.requests.append(rec)
            self.log.write(json.dumps(rec) + '\n')
            await self._route(method, path, body, writer)
        except (asyncio.IncompleteReadError, asyncio.TimeoutError, ConnectionError):
            pass
        finally:
            try:
                writer.close()
            except Exception:  # noqa: BLE001
                pass

    async def _route(self, method, path, body, writer):
        if method == 'GET' and path == '/status':
            writer.write(http_response(200, json.dumps(self._status())))
        elif method == 'GET' and path == '/health':
            writer.write(http_response(200, json.dumps(self._health())))
        elif method == 'GET' and path == '/audit':
            writer.write(http_response(200, '[]'))
        elif method == 'POST' and path == '/write':
            b = self.behaviors.pop(0) if self.behaviors else {'kind': 'ok'}
            await self._write(b, body, writer)
        else:
            writer.write(http_response(404, 'no route', 'text/plain'))
        await writer.drain()

    async def _write(self, b, body, writer):
        kind = b['kind']
        if kind == 'ok':
            req = json.loads(body)
            writer.write(http_response(200, json.dumps({'ok': True, 'name': req['name'], 'value': req['value'], 'undelivered_device': None})))
        elif kind == 'http':
            writer.write(http_response(b['status'], b.get('body', ''), b.get('ctype', 'text/plain'), b.get('headers')))
        elif kind == 'half':
            writer.write(http_response(200, '{"ok": true, "na', length=200))     # promises 200 bytes, sends 16, closes
        elif kind == 'hang':
            await asyncio.sleep(b['seconds'])
        await writer.drain()
