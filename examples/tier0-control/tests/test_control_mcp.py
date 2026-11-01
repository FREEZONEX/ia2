"""Real stdio protocol, persistent requests, authentication and sampler process."""
import argparse
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT), str(Path(__file__).resolve().parent)]
import control_mcp as mcp
from bridge import auth as request_auth, eventauth
from readside import auth
from signing_fixtures import EVENT_KEYS, SAMPLE_KEYS, event_record, sample_record
import test_verifier as tv


class Control(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        entry = dict(key='flow', device_id='d', ia2_variable='sp_flow', type='REAL', min=0, max=40,
                     simulation=True, enabled=True, max_deadline_ms=60000, verify=tv.ENTRY['verify'])
        text = json.dumps(dict(version=1, targets=[entry]))
        (self.root / 'registry.json').write_text(text)
        self.rings = {'keys': {'request-test': dict(secret=b'R' * 32, not_before_ms=0, not_after_ms=2**60)},
                      'event_keys': EVENT_KEYS, 'sample_keys': SAMPLE_KEYS}
        for name, ring in self.rings.items():
            path = self.root / (name + '.json')
            path.write_text(json.dumps({kid: dict(secret_hex=key['secret'].hex(), not_before_ms=key['not_before_ms'],
                                                 not_after_ms=key['not_after_ms']) for kid, key in ring.items()}))
            path.chmod(0o600)
        self.args = argparse.Namespace(registry=str(self.root / 'registry.json'), registry_sha256=hashlib.sha256(text.encode()).hexdigest(),
                                       requests=str(self.root / 'requests.jsonl'), trace=str(self.root / 'trace.jsonl'),
                                       samples=str(self.root / 'samples.jsonl'), revoked_keys=None,
                                       **{name: str(self.root / (name + '.json')) for name in self.rings})
        self.tools = mcp.ControlTools(self.args)

    def process(self):
        args = [sys.executable, '-I', '-B', str(ROOT / 'control_mcp_run.py')]
        for name, value in vars(self.args).items():
            if value is not None:
                args += ['--' + name.replace('_', '-'), value]
        p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def cleanup():
            if p.poll() is None:
                p.kill()
            p.wait(timeout=5)
            for stream in (p.stdin, p.stdout, p.stderr):
                stream.close()
        self.addCleanup(cleanup)
        return p

    def send(self, p, method, params=None, ident=1):
        request = dict(jsonrpc='2.0', method=method, params=params or {})
        if ident is not None:
            request['id'] = ident
        p.stdin.write(json.dumps(request).encode() + b'\n')
        p.stdin.flush()
        if ident is None:
            return None
        with selectors.DefaultSelector() as selector:
            selector.register(p.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(5), 'MCP response timed out')
        line = p.stdout.readline()
        self.assertTrue(line, 'MCP exited unexpectedly')
        result = json.loads(line)
        self.assertEqual(result['id'], ident)
        return result

    def initialize(self, p):
        response = self.send(p, 'initialize', dict(protocolVersion=mcp.PROTOCOL, capabilities={}, clientInfo={'name': 'test', 'version': '1'}))
        self.assertEqual(response['result']['protocolVersion'], mcp.PROTOCOL)
        self.send(p, 'notifications/initialized', ident=None)

    def evidence(self, aid):
        request = json.loads(json.loads(Path(self.args.requests).read_text().splitlines()[-1])['payload'])
        now = request['requested_at_ms']
        event = event_record(dict(tv.ACC, action_id=aid, requested_value=request['target'], requested_at_ms=now,
                                  observed_at_ms=now, sent_at_ms=now, deadline_ms=now + 20000,
                                  scan_anchor_ms=now - 5000, registry_sha256=self.args.registry_sha256))
        sample = sample_record(tv.sample(t=5_100_000, value=request['target'], wall=now + 100))
        sample['collected_at_ms'] = now + 100
        Path(self.args.trace).write_text(json.dumps(eventauth.sign(event, EVENT_KEYS)) + '\n')
        Path(self.args.samples).write_text(json.dumps(auth.sign(sample, SAMPLE_KEYS, 'sample')) + '\n')
        return event, sample

    def test_real_stdio_has_exactly_two_tools_and_survives_restart(self):
        p = self.process()
        self.initialize(p)
        tools = self.send(p, 'tools/list')['result']['tools']
        self.assertEqual([t['name'] for t in tools], ['ia2_set', 'get_action_status'])
        response = self.send(p, 'tools/call', dict(name='ia2_set', arguments={'target_key': 'flow', 'target': 12.5}))['result']
        self.assertFalse(response['isError'])
        submitted = json.loads(response['content'][0]['text'])
        self.assertEqual(submitted['status'], 'requested')
        aid = submitted['action_id']
        line = Path(self.args.requests).read_text().strip()
        payload, _ = request_auth.verify_envelope(line, self.rings['keys'], int(time.time() * 1000))
        self.assertEqual(json.loads(payload)['action_id'], aid)
        self.evidence(aid)
        p.stdin.close()
        self.assertEqual(p.wait(timeout=5), 0)
        other = self.process()
        self.initialize(other)
        response = self.send(other, 'tools/call', dict(name='get_action_status', arguments={'action_id': aid}))['result']
        result = json.loads(response['content'][0]['text'])
        self.assertEqual(result['status'], 'verified')
        self.assertEqual(len(Path(self.args.requests).read_text().splitlines()), 1)
        self.assertNotIn('secret', json.dumps(response))

    def test_value_target_ttl_and_no_arbitrary_operations(self):
        for target, value, ttl in [('unknown', 1, 100), ('flow', 100, 100), ('flow', True, 100),
                                   ('flow', 1, 0), ('flow', 1, True), ('flow', 1, 60001)]:
            with self.subTest(target=target, value=value, ttl=ttl):
                with self.assertRaises(Exception):
                    self.tools.ia2_set(target, value, ttl)
        self.assertFalse(Path(self.args.requests).exists())
        server = mcp.Server(self.tools)
        server.ready = True
        for name, args in [('force', {}), ('ia2_set', {'target_key': 'flow', 'target': 1, 'variable': 'arbitrary'}),
                           ('get_action_status', {'action_id': 'a' * 32, 'url': 'http://remote'})]:
            r = server.dispatch(dict(jsonrpc='2.0', id=1, method='tools/call', params=dict(name=name, arguments=args)))
            self.assertEqual(r['error']['code'], -32602)

    def test_lifecycle_parse_errors_notifications_and_frame_limit(self):
        server = mcp.Server(self.tools)
        r = server.dispatch(dict(jsonrpc='2.0', id=1, method='tools/list'))
        self.assertEqual(r['error']['code'], -32002)
        self.assertIsNone(server.dispatch(dict(jsonrpc='2.0', method='notifications/unknown')))
        self.assertEqual(server.dispatch([])['error']['code'], -32600)
        output = io.BytesIO()
        self.assertEqual(server.run(io.BytesIO(b'{bad}\n'), output), 0)
        self.assertEqual(json.loads(output.getvalue())['error']['code'], -32700)
        self.assertEqual(server.run(io.BytesIO(b' ' * (mcp.MAX_FRAME + 1)), io.BytesIO()), 2)

    def test_forgery_is_counted_and_incremental_reads_do_not_recount(self):
        aid = self.tools.ia2_set('flow', 12.5)['action_id']
        event, sample = self.evidence(aid)
        with open(self.args.samples, 'a') as f:
            f.write(json.dumps(sample) + '\n')
        for _ in range(2):
            r = self.tools.get_action_status(aid)
            self.assertEqual((r['status'], r['rejected_envelopes']), ('verified', 1))
        Path(self.args.samples).write_text(json.dumps(sample) + '\n')
        self.assertNotEqual(self.tools.get_action_status(aid)['status'], 'verified')

    def test_stdio_rejects_non_utf8_json_and_continues(self):
        ping = json.dumps(dict(jsonrpc='2.0', id=1, method='ping')) + '\n'
        for encoding in ('utf-16-be', 'utf-32-be'):
            with self.subTest(encoding=encoding):
                output = io.BytesIO()
                server = mcp.Server(self.tools)
                self.assertEqual(server.run(io.BytesIO(ping.encode(encoding) + ping.encode('utf-8')), output), 0)
                rejected, valid = map(json.loads, output.getvalue().splitlines())
                self.assertEqual(rejected.get('error', {}).get('code'), -32700)
                self.assertEqual(valid, dict(jsonrpc='2.0', id=1, result={}))

    def test_index_waits_for_complete_line_and_rebuilds_on_replacement(self):
        path = Path(self.args.trace)
        event = event_record(tv.ACC)
        line = json.dumps(eventauth.sign(event, EVENT_KEYS)) + '\n'
        path.write_text(line[:-1])
        index = self.tools.trace
        index.refresh()
        self.assertEqual(index.rows, [])
        with path.open('a') as f:
            f.write('\n')
        index.refresh()
        self.assertEqual(len(index.rows), 1)
        replacement = self.root / 'replacement'
        replacement.write_text('bad\n')
        os.replace(replacement, path)
        index.refresh()
        self.assertEqual((index.rows, index.rejected), ([], 1))

    def test_index_budget_never_returns_incomplete_success(self):
        aid = self.tools.ia2_set('flow', 12.5)['action_id']
        self.evidence(aid)
        with mock.patch.object(mcp, 'MAX_RECORDS', 0):
            for _ in range(2):
                with self.assertRaises(ValueError):
                    self.tools.get_action_status(aid)

    def test_missing_unsigned_or_conflicting_requests_cannot_bind_action(self):
        aid = self.tools.ia2_set('flow', 12.5)['action_id']
        self.evidence(aid)
        env = json.loads(Path(self.args.requests).read_text())
        p = json.loads(env['payload'])
        p['target'] = 20
        payload = json.dumps(p)
        env.update(payload=payload, sig=request_auth.sign(b'R' * 32, payload))
        with open(self.args.requests, 'a') as f:
            f.write(json.dumps(env) + '\n')
        with self.assertRaises(ValueError):
            self.tools.get_action_status(aid)
        Path(self.args.requests).write_text(json.dumps(p) + '\n')
        with self.assertRaises(ValueError):
            self.tools.get_action_status(aid)

    def test_ambiguous_append_returns_action_id_and_never_resubmits(self):
        with mock.patch.object(mcp.os, 'fsync', side_effect=OSError('injected failure')):
            r = self.tools.ia2_set('flow', 12.5)
        self.assertEqual(r['status'], 'unknown')
        self.assertEqual(len(Path(self.args.requests).read_text().splitlines()), 1)
        self.assertEqual(self.tools.get_action_status(r['action_id'])['status'], 'requested')

    def test_action_outcomes_and_tool_errors_have_distinct_boundaries(self):
        def call(name, arguments):
            server = mcp.Server(mcp.ControlTools(self.args))
            server.ready = True
            reply = server.dispatch(dict(jsonrpc='2.0', id=1, method='tools/call',
                                         params=dict(name=name, arguments=arguments)))['result']
            return reply['isError'], json.loads(reply['content'][0]['text'])

        aid = self.tools.ia2_set('flow', 12.5)['action_id']
        event, sample = self.evidence(aid)
        with open(self.args.samples, 'a') as stream:
            stream.write(json.dumps(sample) + '\n')
        error, result = call('get_action_status', {'action_id': aid})
        self.assertFalse(error)
        self.assertEqual((result['status'], result['rejected_envelopes']), ('verified', 1))

        event, _ = self.evidence(aid)
        event.update(status='unknown', reason='runtime_unreachable')
        notes = [dict(event, kind='reconcile_note', event_id='note-%d' % i, pass_no=value)
                 for i, value in enumerate((1, '2'))]
        Path(self.args.trace).write_text(''.join(json.dumps(eventauth.sign(row, EVENT_KEYS)) + '\n'
                                              for row in [event] + notes))
        error, result = call('get_action_status', {'action_id': aid})
        self.assertFalse(error)
        self.assertEqual((result['status'], result['reason'], result['rejected_envelopes']),
                         ('unknown', 'no_proof_by_deadline', 0))
        self.assertEqual(result['detail'], 'malformed authenticated evidence')

        with mock.patch.object(mcp, 'MAX_RECORDS', 0):
            error, result = call('get_action_status', {'action_id': aid})
        self.assertTrue(error)
        self.assertEqual(result['error']['code'], 'tool_refused')

        with mock.patch.object(mcp.os, 'fsync', side_effect=OSError('injected uncertainty')):
            error, result = call('ia2_set', {'target_key': 'flow', 'target': 13.5})
        self.assertFalse(error)
        self.assertEqual((result['status'], result['reason']), ('unknown', 'request_persistence_unconfirmed'))
        error, status = call('get_action_status', {'action_id': result['action_id']})
        self.assertFalse(error)
        self.assertEqual(status['status'], 'requested')
        self.assertEqual(len(Path(self.args.requests).read_text().splitlines()), 2)

    def test_malformed_authenticated_data_is_tool_error_and_stdio_survives(self):
        aid = self.tools.ia2_set('flow', 12.5)['action_id']
        original = Path(self.args.requests).read_text()
        for bad in ('deadline', 'target_key', 'runtime_id', 'sample_time'):
            with self.subTest(field=bad):
                Path(self.args.requests).write_text(original)
                event, sample = self.evidence(aid)
                if bad in ('deadline', 'target_key'):
                    env = json.loads(original)
                    request = json.loads(env['payload'])
                    if bad == 'deadline':
                        del request['deadline_ms']
                    else:
                        request['target_key'] = []
                    payload = json.dumps(request)
                    env.update(payload=payload, sig=request_auth.sign(b'R' * 32, payload))
                    Path(self.args.requests).write_text(json.dumps(env) + '\n')
                elif bad == 'runtime_id':
                    event['runtime_id'] = []
                    Path(self.args.trace).write_text(json.dumps(eventauth.sign(event, EVENT_KEYS)) + '\n')
                else:
                    sample.update(collected_at_ms=event['deadline_ms'] + 10000, observed_at_ms=[])
                    Path(self.args.samples).write_text(json.dumps(auth.sign(sample, SAMPLE_KEYS, 'sample')) + '\n')
                p = self.process()
                self.initialize(p)
                response = self.send(p, 'tools/call', dict(name='get_action_status', arguments={'action_id': aid}))['result']
                self.assertTrue(response['isError'])
                error = json.loads(response['content'][0]['text'])['error']
                self.assertEqual(error['code'], 'local_data_error')
                self.assertEqual(self.send(p, 'ping')['result'], {})
                p.stdin.close()
                self.assertEqual(p.wait(timeout=5), 0)
        self.assertEqual(len(Path(self.args.requests).read_text().splitlines()), 1)

    def test_startup_refuses_reused_keys_wrong_pin_or_hardware_registry(self):
        args = copy.copy(self.args)
        args.event_keys = args.keys
        with self.assertRaises(ValueError):
            mcp.ControlTools(args)
        args = copy.copy(self.args)
        args.registry_sha256 = '0' * 64
        with self.assertRaises(Exception):
            mcp.ControlTools(args)
        doc = json.loads(Path(self.args.registry).read_text())
        doc['targets'][0]['simulation'] = False
        text = json.dumps(doc)
        Path(self.args.registry).write_text(text)
        self.args.registry_sha256 = hashlib.sha256(text.encode()).hexdigest()
        with self.assertRaises(ValueError):
            mcp.ControlTools(self.args)

    def test_standalone_sampler_only_gets_status_and_signs_samples(self):
        seen = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                seen.append(('GET', self.path))
                count = len(seen)
                body = dict(runtime_id='R1', last_snapshot=dict(scan_count=count, timestamp_us=count * 1000,
                            vars=[dict(name='sp_flow_fb', type_name='REAL', bits=1095237632,
                                       input=dict(device='coupler', stale=False))]),
                            device_health=[dict(name='coupler', healthy=True)], forces=[], fault=None,
                            watchdog_tripped=False, latch_reason=None, mode={'kind': 'running'})
                data = json.dumps(body).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        command = [sys.executable, '-I', '-B', str(ROOT / 'sampler_run.py'), '--runtime', '127.0.0.1:%d' % server.server_port,
                   '--samples', self.args.samples, '--sample-keys', self.args.sample_keys, '--interval-ms', '50']
        p = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            end = time.monotonic() + 5
            while time.monotonic() < end and (not Path(self.args.samples).exists() or Path(self.args.samples).stat().st_size == 0):
                time.sleep(0.02)
            p.terminate()
            out, err = p.communicate(timeout=5)
            self.assertEqual(p.returncode, 0, err)
            self.assertTrue(json.loads(out)['ready'])
            samples = Path(self.args.samples).read_text().splitlines()
            self.assertTrue(samples)
            record = auth.verify(samples[0], SAMPLE_KEYS, 'sample')
            self.assertEqual(record['native_runtime_id'], 'R1')
            self.assertEqual(set(seen), {('GET', '/status')})
        finally:
            if p.poll() is None:
                p.kill()
                p.communicate(timeout=5)


if __name__ == '__main__':
    unittest.main(verbosity=2)
