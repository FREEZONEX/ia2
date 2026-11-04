"""C11: real MQTT -> sandbox runtime -> fake Modbus feedback and audit.

Only the worker publishes messages. It runs under cs agent run against its own
IDE; the parent keeps the fake devices alive and services one explicit restart.
"""
import asyncio
import datetime
import http.client
import json
import math
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (Checks, FLOW, UNLISTED, MQTT_PORT, RT_PORT, PROXY_PORT,
                     SLAVE_PORT, SLAVE2_PORT, ROOT, Stack, port_free, sha256_file)
from minimqtt import MiniMqttClient
from test_broker import TempBroker


def utc8():
    return datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat()


def save(path, body):
    with open(path, 'w') as f:
        json.dump(body, f, indent=1, sort_keys=True)
        f.write('\n')


def northbound(prefix, allow):
    return ('[mqtt]\nenabled = true\nbroker_host = "127.0.0.1"\nbroker_port = %d\n'
            'topic_prefix = "%s"\npublish_interval_ms = 200\nqos = 1\nallow_write = %s\n'
            % (MQTT_PORT, prefix, str(allow).lower()))


def bits(value):
    return struct.unpack('!i', struct.pack('!f', value))[0]


def variables(body):
    return {v['name']: v for v in body.get('last_snapshot', {}).get('vars', [])}


def mqtt_window_evidence(messages, prefix, start_ms, end_ms, prior, final, max_gap_ms=600):
    """Require advancing runtime telemetry throughout a negative-write observation.

    Broker/PING liveness alone cannot distinguish a disabled writer from a lost
    runtime connection. Compare telemetry with the HTTP snapshots of this same
    instance; duplicates and retained/old-instance snapshots cannot refresh it.
    """
    issues, advancing = [], []
    statuses = [m for m in messages if m.topic == prefix + '/status' and m.received_at_ms <= end_ms]
    before = [m for m in statuses if m.received_at_ms <= start_ms]
    if not before or before[-1].payload != b'online':
        issues.append('runtime was not online at observation start')
    if any(m.payload != b'online' for m in statuses if m.received_at_ms >= start_ms):
        issues.append('runtime announced offline/degraded during observation')
    if prior['runtime_id'] != final['runtime_id']:
        issues.append('runtime identity changed during observation')
    low, high = prior['last_snapshot'], final['last_snapshot']
    last_scan, last_ts = low['scan_count'], low['timestamp_us']
    snapshots = [m for m in messages if m.topic == prefix + '/snapshot'
                 and start_ms <= m.received_at_ms <= end_ms]
    for message in snapshots:
        try:
            body = json.loads(message.payload)
            scan, ts = body['scan'], body['ts_us']
            values = body['values']
            valid = (not message.retain and type(scan) is int and type(ts) is int
                     and scan <= high['scan_count'] and ts <= high['timestamp_us']
                     and all(type(values[n]) in (int, float) and math.isfinite(values[n])
                             for n in ('sp_flow', 'sp_flow_fb'))
                     and body['inputs']['sp_flow_fb']['stale'] is False)
            if not valid:
                issues.append('retained, invalid, stale or mismatched runtime snapshot')
                continue
            if scan > last_scan and ts > last_ts:
                advancing.append(message.received_at_ms)
                last_scan, last_ts = scan, ts
        except (ValueError, KeyError, TypeError):
            issues.append('malformed runtime snapshot')
    boundaries = [start_ms, *advancing, end_ms]
    gap = max(b - a for a, b in zip(boundaries, boundaries[1:]))
    if end_ms <= start_ms or len(advancing) < 2 or gap > max_gap_ms:
        issues.append('runtime telemetry missing or stalled during observation')
    return dict(ok=not issues, issues=sorted(set(issues)), start_ms=start_ms, end_ms=end_ms,
                max_gap_ms=gap, allowed_gap_ms=max_gap_ms, snapshots=len(snapshots),
                advancing_snapshots=len(advancing), runtime_id=prior['runtime_id'])


def request(directory, port, method, path, body=None, project=None):
    headers = {'X-IA2-Origin': 'lab-c11', 'Content-Type': 'application/json'}
    if project:
        headers['X-IA2-Project'] = project
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
    try:
        conn.request(method, path, body=json.dumps(body) if body is not None else None, headers=headers)
        response = conn.getresponse()
        raw = response.read().decode()
        try:
            result = json.loads(raw)
        except ValueError:
            result = raw
        with open(os.path.join(directory, 'http.jsonl'), 'a') as f:
            f.write(json.dumps(dict(time_utc8=utc8(), port=port, method=method, path=path,
                                   headers=headers, body=body, status=response.status,
                                   response_headers=response.getheaders(), response=raw)) + '\n')
        return response.status, result
    finally:
        conn.close()


class MqttStack(Stack):
    def __init__(self, *args, broker_bin, server_bin, cs_bin, **kw):
        super().__init__(*args, **kw)
        self.broker_bin, self.server_bin, self.cs_bin = broker_bin, server_bin, cs_bin
        self.prefix = 'ia2/c11/' + uuid.uuid4().hex
        self.northbound_text = northbound(self.prefix, True)
        self.server = self.server_log = None
        self.worker = None
        self.exits = []

    async def start(self, **kw):
        await super().start(**kw)
        shutil.copyfile(self.p('project/northbound.toml'), self.p('northbound-enabled.toml'))
        self.broker = TempBroker(self.broker_bin, MQTT_PORT, self.p('broker.log'))
        await asyncio.to_thread(self.broker.start)
        os.makedirs(self.p('home'))
        shutil.copytree(os.path.join(ROOT, 'sim'), self.p('sim'))
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            self.server_port = sock.getsockname()[1]
        self.server_env = dict(os.environ, HOME=self.p('home'), XDG_CONFIG_HOME=self.p('home/.config'))
        self.server_env.pop('IA2_ALLOWED_ORIGINS', None)
        self.server_log = open(self.p('server.log'), 'wb')
        command = [self.server_bin, '--bind', '127.0.0.1:%d' % self.server_port, '--demo-modbus-addr', '']
        self.server = subprocess.Popen(command, cwd=self.dir, env=self.server_env,
                                       stdout=self.server_log, stderr=self.server_log)
        for _ in range(100):
            try:
                code, _ = await asyncio.to_thread(request, self.dir, self.server_port, 'GET', '/health')
                if code == 200:
                    break
            except OSError:
                pass
            if self.server.poll() is not None:
                raise RuntimeError('isolated IDE exited during startup')
            await asyncio.sleep(0.1)
        else:
            raise RuntimeError('isolated IDE did not become ready')
        cfg = dict(prefix=self.prefix, runtime_port=RT_PORT, broker_port=MQTT_PORT,
                   server_port=self.server_port, cs_bin=self.cs_bin, started_utc8=utc8(),
                   server_command=command, python=sys.executable, python_version=sys.version,
                   binaries={p: sha256_file(p) for p in (self.bin, self.broker_bin, self.server_bin, self.cs_bin)})
        save(self.p('config.json'), cfg)

    async def _spawn_runtime(self):
        if getattr(self, 'rtlog', None):
            self.rtlog.close()
        await super()._spawn_runtime()

    async def stop(self):
        try:
            if self.worker:
                await stop_worker(self.worker)
            # Stop MQTT/runtime before taking down the isolated IDE.
            await super().stop()
        finally:
            if self.broker:
                self.broker.stop()
            if self.server and self.server.poll() is None:
                self.server.terminate()
                try:
                    await asyncio.to_thread(self.server.wait, 5)
                except subprocess.TimeoutExpired:
                    self.server.kill()
                    await asyncio.to_thread(self.server.wait)
            if self.server_log:
                self.server_log.close()
            ports = [RT_PORT, PROXY_PORT, SLAVE_PORT, SLAVE2_PORT, MQTT_PORT]
            if hasattr(self, 'server_port'):
                ports.append(self.server_port)
            cleanup = dict(time_utc8=utc8(), runtime_exit=self.proc.poll() if self.proc else None,
                           restart_exits=self.exits, server_exit=self.server.poll() if self.server else None,
                           broker_exit=self.broker.proc.poll() if self.broker and self.broker.proc else None,
                           ports_free={str(p): port_free(p) for p in ports})
            save(self.p('cleanup.json'), cleanup)
            if not all(cleanup['ports_free'].values()):
                raise RuntimeError('C11 listener remained after cleanup')


async def stop_worker(child):
    # A private process group owns the wrapper, worker and heartbeat processes.
    # The worker can exit normally while its wrapper still needs cleanup.
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        await asyncio.to_thread(child.wait)
        return
    for _ in range(50):
        child.poll()
        try:
            os.killpg(child.pid, 0)
        except ProcessLookupError:
            return
        await asyncio.sleep(0.1)
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    await asyncio.to_thread(child.wait)


async def case_C11(st, ck, notes):
    log = open(st.p('exercise.log'), 'wb')
    command = [st.cs_bin, '--server', 'http://127.0.0.1:%d' % st.server_port,
               'agent', 'run', '--label', 'C11 loopback MQTT characterization', '--',
               sys.executable, '-I', '-B', os.path.abspath(__file__), '--worker', st.dir]
    save(st.p('exercise-command.json'), dict(command=command, started_utc8=utc8()))
    st.worker = subprocess.Popen(command, stdout=log, stderr=log, env=st.server_env, start_new_session=True)
    deadline, restarted = time.monotonic() + 120, False
    try:
        while st.worker.poll() is None:
            if os.path.exists(st.p('restart-request.json')) and not restarted:
                restarted = True
                # Preserve enabled-stage evidence, then stop only our runtime.
                await st.snapshot('enabled-final')
                with open(st.p('project/northbound.toml'), 'w') as f:
                    f.write(northbound(st.prefix, False))
                shutil.copyfile(st.p('project/northbound.toml'), st.p('northbound-disabled.toml'))
                code, body = await st.rt('POST', '/stop', {})
                exit_code = await st._wait_runtime_exit(timeout=25)
                st.exits.append(exit_code)
                save(st.p('restart-stop.json'), dict(status=code, body=body, exit=exit_code))
                if code != 200 or exit_code != 0:
                    raise RuntimeError('sandbox did not stop normally for disabled comparison')
                await st._spawn_runtime()
                save(st.p('restart-done.json'), dict(time_utc8=utc8()))
            if time.monotonic() > deadline:
                raise TimeoutError('C11 worker exceeded 120 seconds')
            await asyncio.sleep(0.05)
        save(st.p('exercise-exit.json'), dict(exit=st.worker.returncode, time_utc8=utc8()))
        ck('session-wrapped MQTT exercise exits zero', st.worker.returncode == 0)
        with open(st.p('exercise.json')) as f:
            result = json.load(f)
        ck.items.extend(result['checks'])
        notes.update(result['notes'])
        with open(st.p('matrix.json')) as f:
            rows = json.load(f)
        for row in rows:
            sent, expected = row['sent_at_ms'] / 1000.0, row['expected_applied']
            writes = st.slave_writes(row['address'], sent)
            if expected is None:
                denied_words = list(struct.unpack('!HH', struct.pack('!f', row['value'])))
                ck(row['part'] + ' fake device never receives denied value',
                   not any(w.get('words') == denied_words for w in writes), writes)
            else:
                words = list(struct.unpack('!HH', struct.pack('!i', expected)))
                ck(row['part'] + ' fake device receives applied words',
                   any(w.get('words') == words for w in writes), writes)
        ck('bridge and sampler absent throughout C11', st.bridge is None and st.sampler is None)
        ck('controlled restart was performed once', restarted and st.exits == [0], st.exits)
        with open(st.p('config.json')) as f:
            cfg = json.load(f)
        ck('all tested binary files unchanged during execution',
           all(sha256_file(p) == digest for p, digest in cfg['binaries'].items()))
    finally:
        await stop_worker(st.worker)
        log.close()


def exercise(directory):
    with open(os.path.join(directory, 'config.json')) as f:
        cfg = json.load(f)
    ck, notes, rows, sent = Checks(), {}, [], []
    obs = MiniMqttClient('127.0.0.1', cfg['broker_port'], 'c11-obs-' + uuid.uuid4().hex[:8])
    pub = MiniMqttClient('127.0.0.1', cfg['broker_port'], 'c11-pub-' + uuid.uuid4().hex[:8])
    prefix = cfg['prefix']
    def rt(path):
        code, body = request(directory, cfg['runtime_port'], 'GET', path)
        if code != 200:
            raise RuntimeError('runtime read failed: %s %r' % (path, body))
        return body
    def run_cli(tag, *args):
        command = [cfg['cs_bin'], '--server', 'http://127.0.0.1:%d' % cfg['server_port'], *args]
        result = subprocess.run(command, capture_output=True, text=True, timeout=40)
        save(os.path.join(directory, tag + '.json'), dict(command=command, exit=result.returncode,
                                                       stdout=result.stdout, stderr=result.stderr))
        ck(tag, result.returncode == 0, result.stderr)
    try:
        code, projects = request(directory, cfg['server_port'], 'GET', '/api/projects')
        ck('isolated IDE has no preexisting projects', code == 200 and projects == [], projects)
        for path, name in [('project', 'control_lab'), ('sim', 'control_bridge_smoke')]:
            code, body = request(directory, cfg['server_port'], 'POST', '/api/projects/open',
                                 {'path': os.path.join(directory, path)})
            ck(path + ' opens in isolated IDE', code == 200, body)
            run_cli(path + '-check', '--project', name, 'project', 'check', os.path.join(directory, path))
        run_cli('sim-smoke', '--project', 'control_bridge_smoke', 'sim', 'run',
                os.path.join(directory, 'sim/scenarios/setpoints.toml'))
        obs.connect()
        obs.subscribe(prefix + '/#', qos=1)
        pub.connect()
        obs.wait_for_message(prefix + '/status', lambda m: m.payload == b'online', timeout=12)
        obs.wait_for_message(prefix + '/snapshot', timeout=12)
        obs.ping()
        health, initial = rt('/health'), rt('/status')
        ck('runtime scans with expected project and two healthy devices',
           initial.get('project') == 'control_lab' and initial.get('scan_count', 0) > 0
           and bool(health.get('devices')) and all(d.get('healthy') for d in health['devices']), health)
        ck('initial audit is empty', rt('/audit') == [])
        save(os.path.join(directory, 'initial.json'), dict(health=health, status=initial, audit=rt('/audit')))
        window_start = utc8()
        for part, name, value, applied, outcome, address in [
            ('original', 'sp_flow', 12.5, bits(12.5), 'ok', 100),
            ('governance-denied', 'unlisted_sp', 5.0, None, None, 140),
            ('governance-clamped', 'sp_flow', 50.0, bits(40.0), 'clamped', 100),
        ]:
            before, prior = rt('/audit'), rt('/status')
            payload = json.dumps(dict(name=name, value=value))
            record = dict(part=part, topic=prefix + '/write', payload=payload, qos=1, retain=False,
                          sent_at_ms=int(time.time() * 1000), sent_utc8=utc8())
            sent.append(record)
            pub.publish(record['topic'], payload, qos=1, retain=False)
            record['puback_utc8'] = utc8()  # Delivery acknowledgement only.
            end = time.monotonic() + 6
            while True:
                after = rt('/audit')
                if len(after) > len(before) or time.monotonic() > end:
                    break
                time.sleep(0.05)
            entry = after[-1] if len(after) > len(before) else {}
            ck(part + ' exactly one new audit', len(after) == len(before) + 1 and after[:-1] == before, after)
            ck(part + ' complete audit identity and values',
               [entry.get(k) for k in ('origin', 'op', 'name', 'requested', 'applied')]
               == ['mqtt', 'write', name, bits(value), applied], entry)
            ck(part + ' governance outcome',
               entry.get('result') == outcome if outcome else
               'rejected by project governance' in entry.get('result', ''), entry)
            wanted = applied if applied is not None else variables(prior)[name]['bits']
            end = time.monotonic() + 6
            while True:
                status = rt('/status')
                vs = variables(status)
                if (vs.get(name, {}).get('bits') == wanted
                        and status['scan_count'] > prior['scan_count']
                        and (name != 'sp_flow' or vs.get('sp_flow_fb', {}).get('bits') == wanted)):
                    break
                if time.monotonic() > end:
                    break
                time.sleep(0.05)
            ck(part + ' independent snapshot and feedback',
               vs.get(name, {}).get('bits') == wanted and status['scan_count'] > prior['scan_count']
               and (name != 'sp_flow' or vs.get('sp_flow_fb', {}).get('bits') == wanted), vs)
            # A bounded window also detects duplicate audits after the first one.
            time.sleep(0.4)
            ck(part + ' no extra audit after settling', rt('/audit') == after)
            rows.append(dict(part=part, name=name, value=value, expected_applied=applied, address=address,
                             sent_at_ms=record['sent_at_ms'], audit_before=before, audit_after=after, status=status))
        # Require a fresh live snapshot after the last write and a live observer.
        marker = int(time.time() * 1000)
        time.sleep(1.5)
        obs.ping()
        ck('observer still receives fresh snapshots during final observation',
           any(m.topic == prefix + '/snapshot' and m.received_at_ms >= marker for m in obs.messages()))
        topics = sorted({m.topic for m in obs.messages()})
        stray = sorted(set(topics) - {prefix + '/write', prefix + '/status', prefix + '/snapshot'})
        ck('no per-request result or error topic within observed prefix and window', not stray, topics)
        ck('observer saw all three enabled write publications',
           sum(m.topic == prefix + '/write' for m in obs.messages()) == 3)
        notes['response_observation'] = dict(start_utc8=window_start, end_utc8=utc8(),
                                             scope=prefix + '/#', topics=topics, stray_topics=stray,
                                             final_quiet_seconds=1.5)
        old_runtime = rt('/status')['runtime_id']
        restart_marker = int(time.time() * 1000)
        save(os.path.join(directory, 'restart-request.json'), dict(time_utc8=utc8()))
        deadline = time.monotonic() + 35
        while not os.path.exists(os.path.join(directory, 'restart-done.json')):
            if time.monotonic() > deadline:
                raise TimeoutError('parent did not restart sandbox')
            time.sleep(0.05)
        # Retained status from the previous runtime is insufficient readiness.
        # Require new runtime identity and a newly delivered online + snapshot.
        fresh = rt('/status')
        ck('disabled comparison uses a new runtime identity', fresh['runtime_id'] != old_runtime)
        marker = int(time.time() * 1000)
        obs.wait_for_message(prefix + '/snapshot', lambda m: m.received_at_ms >= marker, timeout=8)
        obs.wait_for_message(prefix + '/status', lambda m: m.payload == b'online' and not m.retain
                             and m.received_at_ms >= restart_marker, timeout=8)
        before = rt('/audit')
        prior = rt('/status')
        ck('disabled runtime begins with empty audit', before == [])
        payload = json.dumps(dict(name='sp_flow', value=25.0))
        record = dict(part='disabled', topic=prefix + '/write', payload=payload, qos=1, retain=False,
                      sent_at_ms=int(time.time() * 1000), sent_utc8=utc8())
        sent.append(record)
        pub.publish(record['topic'], payload, qos=1, retain=False)
        record['puback_utc8'] = utc8()
        time.sleep(2)
        obs.ping()
        observation_end = int(time.time() * 1000)
        after, final = rt('/audit'), rt('/status')
        liveness = mqtt_window_evidence(obs.messages(), prefix, record['sent_at_ms'], observation_end, prior, final)
        notes['disabled_mqtt_liveness'] = liveness
        ck('disabled runtime MQTT stays live throughout observation', liveness['ok'], liveness)
        ck('disabled write produces no audit during 2 second window', before == after == [], after)
        ck('disabled write changes neither setpoint nor feedback during advancing scans',
           final['scan_count'] > prior['scan_count'] and
           all(variables(final)[n]['bits'] == variables(prior)[n]['bits'] for n in ('sp_flow', 'sp_flow_fb')), final)
        ck('disabled publication reached independent broker observer',
           any(m.topic == prefix + '/write' and m.text == payload and m.received_at_ms >= record['sent_at_ms']
               for m in obs.messages()))
        rows.append(dict(part='disabled', name='sp_flow', value=25.0, expected_applied=None, address=100,
                         sent_at_ms=record['sent_at_ms'], audit_before=before, audit_after=after, status=final))
        ck('final runtime has no faults, forces or watchdog trip',
           not final.get('fault') and not final.get('forces') and not final.get('watchdog_tripped'), final)
        alarms = rt('/alarms')
        ck('no standing runtime alarms', isinstance(alarms, list) and
           all(not a.get('active') and a.get('acked') for a in alarms), alarms)
        notes['parts'] = dict(original='see original checks', governance='enhancement',
                              disabled='enhancement; 2 second observation',
                              bridge_guard='separate C10 regression; bridge never started by C11')
    finally:
        for client in (pub, obs):
            client.disconnect()
        with open(os.path.join(directory, 'mqtt-messages.jsonl'), 'w') as f:
            for m in obs.messages():
                f.write(json.dumps(dict(topic=m.topic, payload=m.text, qos=m.qos, retain=m.retain,
                                        received_at_ms=m.received_at_ms)) + '\n')
        with open(os.path.join(directory, 'mqtt-sent.jsonl'), 'w') as f:
            for record in sent:
                f.write(json.dumps(record) + '\n')
        save(os.path.join(directory, 'matrix.json'), rows)
        save(os.path.join(directory, 'exercise.json'), dict(checks=ck.items, notes=notes))
    return bool(ck.failed)


CASES = {'C11': (case_C11, [FLOW, UNLISTED], {'no_bridge': True, 'with_broker': True})}

if __name__ == '__main__':
    if len(sys.argv) != 3 or sys.argv[1] != '--worker':
        raise SystemExit('use the lab runner, or --worker <case directory>')
    raise SystemExit(exercise(os.path.abspath(sys.argv[2])))
