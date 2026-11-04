"""A3b: real IDE HTTP -> generated SSH/curl -> loopback runtime, no bridge/hardware.

The exercise runs under cs agent run in a worker so the parent's recording
proxy can keep serving. Every audit is read directly from the runtime.
"""
import asyncio
import datetime
import http.client
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import time
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import Checks, ROOT, RT_PORT, PROXY_PORT, Stack, port_free, sha256_file
from recproxy import RecordingProxy
from standin import read_jsonl


def save(path, value):
    with open(path, 'w') as f:
        json.dump(value, f, indent=1, sort_keys=True)
        f.write('\n')


async def off_loop(fn, *args):
    return await asyncio.get_running_loop().run_in_executor(None, lambda: fn(*args))


class ProxyStack(Stack):
    def __init__(self, *args, server_bin, cs_bin, **kw):
        super().__init__(*args, **kw)
        self.server_bin, self.cs_bin = server_bin, cs_bin
        self.server = self.server_log = None

    def _rt_sync(self, method, path, body, timeout):
        try:
            return request(self.dir, RT_PORT, method, path, body, 'lab-harness')
        except OSError as exc:
            return None, repr(exc)

    async def _wait_runtime(self):
        for _ in range(100):
            status, body = await self.rt('GET', '/status', timeout=1)
            if (self.proc.poll() is None and status == 200 and body.get('project') == self.project_name
                    and body.get('mode', {}).get('kind') == 'running' and body.get('scan_count', 0) > 0):
                return True
            if self.proc.poll() is not None:
                return False
            await asyncio.sleep(0.1)
        return False

    async def start(self, **kw):
        os.makedirs(self.dir)
        for port in (RT_PORT, PROXY_PORT):
            if not port_free(port):
                raise RuntimeError('port %d occupied; refusing to run' % port)
        self.project_name = 'control_a3b_' + uuid.uuid4().hex[:8]
        shutil.copytree(os.path.join(ROOT, 'sim'), self.p('project'))
        path = self.p('project/project.toml')
        with open(path) as f:
            text = f.read()
        with open(path, 'w') as f:
            f.write(text.replace('control_bridge_smoke', self.project_name))
        path = self.p('project/pous/main.st')
        with open(path) as f:
            text = f.read()
        with open(path, 'w') as f:
            f.write(text.replace('    sp_flow : REAL;', '    unlisted_sp : DINT;\n    sp_flow : REAL;'))
        os.makedirs(self.p('project/edges'))
        with open(self.p('project/edges/a3b.toml'), 'w') as f:
            f.write('name = "a3b"\nhost = "must-not-resolve.invalid"\nssh_user = "a3b"\nssh_port = 2222\nruntime_port = %d\n' % PROXY_PORT)
        os.makedirs(self.p('mock-bin'))
        with open(os.path.join(ROOT, 'lab/fake_ssh_a3b.py')) as f:
            fake = f.read()
        with open(self.p('mock-bin/ssh'), 'w') as f:
            f.write('#!%s\n' % sys.executable + fake)
        os.chmod(self.p('mock-bin/ssh'), 0o700)
        os.makedirs(self.p('home'))
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0))
            self.server_port = s.getsockname()[1]
        env = dict(os.environ, HOME=self.p('home'), XDG_CONFIG_HOME=self.p('home/.config'),
                   PATH=self.p('mock-bin'), IA2_A3B_PORT=str(PROXY_PORT), IA2_A3B_SSH_LOG=self.p('ssh.jsonl'))
        env.pop('IA2_ALLOWED_ORIGINS', None)
        server_cmd = [self.server_bin, '--bind', '127.0.0.1:%d' % self.server_port, '--demo-modbus-addr', '']
        save(self.p('config.json'), {'server_port': self.server_port, 'runtime_port': RT_PORT, 'proxy_port': PROXY_PORT,
                                    'project': self.project_name, 'server_command': server_cmd,
                                    'runtime_command': [self.bin, '--project-dir', self.p('project'), '--bind',
                                                        '127.0.0.1:%d' % RT_PORT, '--state-dir', self.p('state')],
                                    'server_env': {k: env[k] for k in ('HOME', 'XDG_CONFIG_HOME', 'PATH', 'IA2_A3B_PORT', 'IA2_A3B_SSH_LOG')},
                                    'binaries': {p: sha256_file(p) for p in (self.bin, self.server_bin, self.cs_bin)},
                                    'started_utc8': datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat()})
        await self._spawn_runtime()
        self.proxy = RecordingProxy(PROXY_PORT, RT_PORT, self.p('wire.jsonl'))
        await self.proxy.start()
        self.server_log = open(self.p('server.log'), 'wb')
        self.server = subprocess.Popen(server_cmd, cwd=self.dir, env=env, stdout=self.server_log, stderr=self.server_log)
        for _ in range(100):
            try:
                code, _ = await off_loop(request, self.dir, self.server_port, 'GET', '/health')
                if code == 200:
                    return
            except OSError:
                pass
            if self.server.poll() is not None:
                break
            await asyncio.sleep(0.1)
        raise RuntimeError('IDE server did not become healthy')

    async def stop(self):
        try:
            if self.server and self.server.poll() is None:
                self.server.terminate()
                try:
                    await off_loop(self.server.wait, 10)
                except subprocess.TimeoutExpired:
                    self.server.kill()
                    await off_loop(self.server.wait)
        finally:
            if self.server_log:
                self.server_log.close()
            await super().stop()
            ports = [RT_PORT, PROXY_PORT] + ([self.server_port] if hasattr(self, 'server_port') else [])
            cleanup = {'ports_free': {str(p): port_free(p) for p in ports},
                       'runtime_exit': self.proc.poll() if self.proc else None,
                       'server_exit': self.server.poll() if self.server else None}
            save(self.p('cleanup.json'), cleanup)
            if not all(cleanup['ports_free'].values()):
                raise RuntimeError('test listener remained after cleanup')


def request(directory, port, method, path, body=None, origin=None, project=None):
    headers = {'Content-Type': 'application/json'}
    if origin is not None:
        headers['X-IA2-Origin'] = origin
    if project:
        headers['X-IA2-Project'] = project
    data = json.dumps(body) if body is not None else None
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=8)
    try:
        conn.request(method, path, body=data, headers=headers)
        resp = conn.getresponse()
        raw = resp.read().decode()
        try:
            result = json.loads(raw)
        except ValueError:
            result = raw
        with open(os.path.join(directory, 'http.jsonl'), 'a') as f:
            f.write(json.dumps({'time_utc8': datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat(),
                                'port': port, 'method': method, 'path': path, 'headers': headers, 'body': data,
                                'status': resp.status, 'response_headers': resp.getheaders(), 'response': raw}) + '\n')
        return resp.status, result
    finally:
        conn.close()


def exercise(directory):
    with open(os.path.join(directory, 'config.json')) as f:
        cfg = json.load(f)
    ck, rows = Checks(), []
    def call(port, method, path, body=None, origin=None):
        return request(directory, port, method, path, body, origin, cfg['project'])
    ide, rt = cfg['server_port'], cfg['runtime_port']
    code, projects = call(ide, 'GET', '/api/projects')
    ck('isolated IDE starts with no projects', code == 200 and projects == [], projects)
    code, body = call(ide, 'POST', '/api/projects/open', {'path': os.path.join(directory, 'project')}, 'lab-a3b')
    ck('test project opened', code == 200, body)
    code, projects = call(ide, 'GET', '/api/projects')
    ck('only test project is open', code == 200 and len(projects) == 1 and projects[0]['name'] == cfg['project'], projects)
    code, body = call(ide, 'POST', '/api/project/validate', {}, 'lab-a3b')
    ck('test project validates', code == 200 and body == [], body)
    cs = next(p for p in cfg['binaries'] if os.path.basename(p) == 'cs')
    for tag, command in [
        ('project-check', [cs, '--server', 'http://127.0.0.1:%d' % ide, '--project', cfg['project'],
                           'project', 'check', os.path.join(directory, 'project')]),
        ('sim-smoke', [cs, '--server', 'http://127.0.0.1:%d' % ide, '--project', cfg['project'],
                      'sim', 'run', os.path.join(directory, 'project/scenarios/setpoints.toml')])]:
        result = subprocess.run(command, capture_output=True, text=True, timeout=30)
        save(os.path.join(directory, tag + '.json'), {'command': command, 'exit': result.returncode,
                                                     'stdout': result.stdout, 'stderr': result.stderr})
        ck(tag + ' passes inside agent session', result.returncode == 0, result.stderr)
    # Verify persistence follows the same isolation contract as sim_e2e.rs.
    home = os.path.join(directory, 'home')
    state = os.path.join(home, 'Library/Application Support' if sys.platform == 'darwin' else '.config', 'IA2/state.toml')
    with open(state) as f:
        ck('IDE persists last_opened inside isolated home', os.path.join(directory, 'project') in f.read())
    labels = [('tier0.lab.1234abcd', 'tier0.lab.1234abcd'), ('lab.a-b_c', 'lab.a-b_c'), ('L'*64, 'L'*64),
              ('lab:colon', 'labcolon'), ('lab space', 'labspace'), ('L'*70, 'L'*64), ('', 'anonymous'),
              (None, 'anonymous'), ('!!!', 'anonymous')]
    cases = [(label, cleaned, 'sp_flow', i + 1.0, None) for i, (label, cleaned) in enumerate(labels)]
    # Extra coverage, separate from original A3b: clamp and governance refusal.
    cases += [('tier0.lab.clamp', 'tier0.lab.clamp', 'sp_flow', 55.0, 'clamp'),
              ('tier0.lab.denied', 'tier0.lab.denied', 'unlisted_sp', 123, 'denied')]
    import struct
    def bits(x):
        return struct.unpack('<i', struct.pack('<f', x))[0]
    for i, (label, cleaned, name, value, extra) in enumerate(cases):
        for route in ('direct', 'proxy'):
            # Distinct per route, including denied requests; no timestamp pairing.
            value_sent = value + ((0.25 if name == 'sp_flow' else 1) if route == 'proxy' else 0)
            requested = bits(value_sent) if name == 'sp_flow' else value_sent
            applied = None if extra == 'denied' else bits(40.0) if extra == 'clamp' else requested
            expected_status = 403 if extra == 'denied' else 200
            expected_result = ("write to '%s (write_mode = allowlist, no rule allows it)' rejected by project governance" % name
                               if extra == 'denied' else 'clamped' if extra == 'clamp' else 'ok')
            code, before = call(rt, 'GET', '/audit')
            ck('%d %s audit before readable' % (i, route), code == 200 and isinstance(before, list))
            _, prior = call(rt, 'GET', '/status')
            port, path = (rt, '/write') if route == 'direct' else (ide, '/api/edges/a3b/runtime/write')
            response = call(port, 'POST', path, {'name': name, 'value': requested}, label)
            code, after = call(rt, 'GET', '/audit')
            entry = after[-1] if after else {}
            tag = '%d %s' % (i, route)
            ck(tag + ' status', response[0] == expected_status, response)
            ck(tag + ' exactly one new audit entry', code == 200 and len(after) == len(before) + 1 and after[:-1] == before, after)
            expected_origin = (label or 'anonymous') if route == 'direct' else cleaned
            ck(tag + ' complete audit tuple',
               [entry.get(k) for k in ('op', 'origin', 'name', 'requested', 'applied', 'result')] ==
               ['write', expected_origin, name, requested, applied, expected_result], entry)
            ck(tag + ' no http_origin', 'http_origin' not in entry, entry)
            wanted = applied if applied is not None else next(v['bits'] for v in prior['last_snapshot']['vars'] if v['name'] == name)
            snapshot = None
            for _ in range(100):
                code, snapshot = call(rt, 'GET', '/status')
                found = next((v for v in snapshot['last_snapshot']['vars'] if v['name'] == name), {})
                if code == 200 and found.get('bits') == wanted and snapshot['scan_count'] > prior['scan_count']:
                    break
                time.sleep(0.02)
            ck(tag + ' independent runtime variable after advancing scan', found.get('bits') == wanted and snapshot['scan_count'] > prior['scan_count'], found)
            rows.append({'label': label, 'route': route, 'enhancement': extra, 'response': response,
                         'audit_before': before, 'audit_after': after, 'status_after': snapshot})
        ck('direct/proxy response passthrough %d' % i, rows[-1]['response'] == rows[-2]['response'] if extra == 'denied' else
           all(r['response'][1].get('ok') is True and r['response'][1].get('value') == r['audit_after'][-1]['applied'] for r in rows[-2:]), rows[-1]['response'])
    ssh = read_jsonl(os.path.join(directory, 'ssh.jsonl'))
    wire = [r for r in read_jsonl(os.path.join(directory, 'wire.jsonl')) if r.get('ev') == 'req']
    proxy_rows = [r for r in rows if r['route'] == 'proxy']
    ck('exactly one real SSH/curl and wire write per proxy row', len(ssh) == len(wire) == len(proxy_rows), (len(ssh), len(wire)))
    for i, (s, w, row) in enumerate(zip(ssh, wire, proxy_rows)):
        entry = row['audit_after'][-1]
        raw_body, _, raw_status = s.get('stdout', '').rpartition('\n')
        try:
            raw_body = json.loads(raw_body)
        except ValueError:
            pass
        ck('SSH/wire/audit match %d' % i, s.get('exit') == 0 and not s.get('rejected') and w['path'] == '/write' and w['method'] == 'POST'
           and (w['origin'] or 'anonymous') == entry['origin'] and json.loads(w['body']) == {'name': entry['name'], 'value': entry['requested']}
           and raw_status == str(row['response'][0]) and raw_body == row['response'][1], (s, w))
    code, status = call(rt, 'GET', '/status')
    ck('runtime healthy and no forces at handoff', code == 200 and status['mode']['kind'] == 'running' and not status.get('forces') and not status.get('watchdog_tripped') and not status.get('fault'), status)
    code, alarms = call(rt, 'GET', '/alarms')
    ck('no standing test alarms', code == 200 and alarms == [], alarms)
    save(os.path.join(directory, 'matrix.json'), rows)
    save(os.path.join(directory, 'exercise.json'), {'checks': ck.items})
    return bool(ck.failed)


async def stop_exercise_group(child, grace_seconds=5):
    """Clean up the private process group created with start_new_session=True."""
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        await off_loop(child.wait, 5)
        return
    deadline = time.monotonic() + grace_seconds
    while True:
        # Reap the wrapper, but do not confuse its exit with group exit.
        child.poll()
        try:
            os.killpg(child.pid, 0)
        except ProcessLookupError:
            break
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass  # The last group member exited between probe and kill.
            break
        await asyncio.sleep(min(0.05, remaining))
    # Only the direct child can be reaped here; orphan descendants are reaped
    # by the OS. SIGKILL above also covers workers that ignore SIGTERM.
    await off_loop(child.wait, 5)


async def case_A3b(st, ck, notes):
    cmd = [st.cs_bin, '--server', 'http://127.0.0.1:%d' % st.server_port, '--project', st.project_name,
           'agent', 'run', '--label', 'Tier0 A3b local proxy labels', '--', sys.executable, '-I', '-B',
           os.path.abspath(__file__), '--exercise', st.dir]
    save(st.p('exercise-command.json'), cmd)
    with open(st.p('exercise.log'), 'wb') as log:
        child = subprocess.Popen(cmd, stdout=log, stderr=log, start_new_session=True)
        try:
            code = await off_loop(child.wait, 120)
        finally:
            # cs owns the exercise/heartbeat children. On failure or timeout,
            # drain the whole group created by this test, including its child.
            await stop_exercise_group(child)
    save(st.p('exercise-exit.json'), {'exit': code})
    ck('session-wrapped exercise exit 0', code == 0, code)
    with open(st.p('exercise.json')) as f:
        for item in json.load(f)['checks']:
            ck(item['name'], item['ok'], item['detail'])
    notes['coverage'] = '9 labels x direct/proxy; extra clamp and governance-denial pairs. No signing bridge, real SSH or hardware.'
    notes['server_bin_sha256'] = sha256_file(st.server_bin)


CASES = {'A3b': (case_A3b, [], {'stack': ProxyStack})}

if __name__ == '__main__':
    if len(sys.argv) != 3 or sys.argv[1] != '--exercise':
        raise SystemExit('internal entry: --exercise CASE_DIRECTORY')
    raise SystemExit(exercise(sys.argv[2]))
