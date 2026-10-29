#!/usr/bin/env python3
"""Slice harness: runs TEST-CASES A1, B1, B3, C1 against a sandbox ia2-runtime, a fake Modbus slave, the real bridge
process, a recording proxy, a sampler thread and the independent verifier.  Loopback only; the only processes it signals
are its own children.  Python >= 3.8, stdlib.

  python3 -I lab/harness.py --bin <ia2-runtime> --out runs [--cases A1,B1,B3,C1]

Per case it writes runs/<ID>-r<n>/ with every input and every log, `verdict.json`, and MANIFEST.sha256.
Verdicts follow TEST-CASES section 2: PASS, FAIL (bridge or contract wrong), FINDING (IA2 differs from the EXPECT)."""
import argparse
import asyncio
import hashlib
import http.client
import json
import os
import platform
import signal
import socket
import subprocess
import sys
import time

sys.dont_write_bytecode = True
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
for p in (ROOT, os.path.join(ROOT, 'lab'), os.path.join(ROOT, 'vendor-copies')):
    sys.path.insert(0, p)
import mkproject_ctl  # noqa: E402
import verifier  # noqa: E402
from fakemodbus_plus import FakeSlavePlus  # noqa: E402
from recproxy import RecordingProxy  # noqa: E402
from sampler import Sampler  # noqa: E402
from stubia2 import StubIA2  # noqa: E402
from standin import Standin, read_jsonl, read_trace  # noqa: E402

RT_PORT, PROXY_PORT, SLAVE_PORT, SLAVE2_PORT, STUB_PORT = 13501, 13502, 13511, 13512, 13503
BRIDGE_RUN = os.path.join(ROOT, 'bridge_run.py')
HOOKS_SKIP_G8 = os.path.join(ROOT, 'lab', 'hooks_skip_g8.py')


def now_ms():
    return int(time.time() * 1000)


def sha256_file(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


def port_free(port):
    with socket.socket() as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(('127.0.0.1', port))
            return True
        except OSError:
            return False


def target(key, var, typ, lo, hi, verify=None, **kw):
    t = {'key': key, 'device_id': 'bench01-sim', 'ia2_variable': var, 'type': typ, 'min': lo, 'max': hi,
         'simulation': True, 'enabled': True}
    if verify:
        t['verify'] = verify
    t.update(kw)
    return t


FLOW = target('flow_setpoint', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'],
              verify={'feedback': 'sp_flow_fb', 'tolerance': 0.05, 'settle_ms': 5000})
UNLISTED = target('unlisted', 'unlisted_sp', 'REAL', 0.0, 10.0, require_devices=['coupler'])


class Checks:
    def __init__(self):
        self.items = []

    def __call__(self, name, ok, detail=''):
        self.items.append({'name': name, 'ok': bool(ok), 'detail': str(detail)[:600]})
        return bool(ok)

    @property
    def failed(self):
        return [c for c in self.items if not c['ok']]


async def wait_until(pred, timeout, step=0.05):
    end = time.time() + timeout
    while time.time() < end:
        v = pred()
        if v:
            return v
        await asyncio.sleep(step)
    return pred()


def verify_chain(path):
    """Re-verify the journal's hash chain from the file alone."""
    prev, n = '0' * 64, 0
    with open(path, 'rb') as f:
        for raw in f.read().split(b'\n')[:-1]:
            rec = json.loads(raw.decode('utf-8'))
            if rec['prev'] != prev or rec['seq'] != n + 1:
                return False, n
            prev, n = hashlib.sha256(raw).hexdigest(), n + 1
    return True, n


class Stack:
    """One complete lab: slaves, sandbox runtime, recording proxy, registry, keys, bridge process, sampler.
    `stub=True` replaces slaves/runtime/proxy/sampler by a scripted stand-in for the runtime's HTTP API (case B11)."""

    def __init__(self, case_dir, binpath, targets, northbound_text=None, stub=False, bridge_args=None, prod=False, project_opts=None):
        self.dir, self.bin, self.targets, self.northbound_text, self.stub = case_dir, binpath, targets, northbound_text, stub
        self.prod, self.bridge_run = prod, BRIDGE_RUN
        self.project_opts = dict(project_opts or {})       # extra mkproject_ctl.render() arguments (P1 cases)
        self.bridge_args = list(bridge_args or [])
        self.startup_windows = []          # (spawn, ready) of every bridge start: its preflight and recovery reads are not request traffic
        self.proc = self.bridge = self.sampler = self.proxy = self.stub_srv = None
        self.slaves, self.slave_events, self.slave_files = {}, {1: [], 2: []}, {}
        self.bridge_n = 0
        self.runtime_n = 0

    def p(self, name):
        return os.path.join(self.dir, name)

    @property
    def slave(self):
        return self.slaves.get(1)

    @property
    def slave2(self):
        return self.slaves.get(2)

    # ---- lifecycle -------------------------------------------------------------------------------------------------------
    async def start(self, hooks=None, env=None):
        os.makedirs(self.dir)
        if self.prod:                                  # run the PRODUCTION package of the bridge instead of the lab tree
            sys.path.insert(0, os.path.join(ROOT, 'tools'))
            import package_prod
            problems = package_prod.build(self.p('prod-package'))
            if problems:
                raise RuntimeError('production package: %s' % problems)
            self.bridge_run = self.p('prod-package/bridge_run.py')
        ports = (STUB_PORT,) if self.stub else (RT_PORT, PROXY_PORT, SLAVE_PORT, SLAVE2_PORT)
        for port in ports:
            if not port_free(port):
                raise RuntimeError('port %d is not free; refusing to run' % port)
        if self.stub:
            os.makedirs(self.p('project'))
            self.stub_srv = StubIA2(STUB_PORT, self.p('stub.jsonl'))
            await self.stub_srv.start()
        else:
            hashes = mkproject_ctl.render(self.p('project'), SLAVE_PORT, SLAVE2_PORT, northbound_text=self.northbound_text,
                                          **self.project_opts)
            with open(self.p('project.sha256.json'), 'w') as f:
                json.dump(hashes, f, indent=1, sort_keys=True)
            for role, port in ((1, SLAVE_PORT), (2, SLAVE2_PORT)):
                self.slave_files[role] = open(self.p('slave-%d.jsonl' % role), 'w', buffering=1)

                def make_log(r):
                    def log(rec):
                        self.slave_events[r].append(rec)
                        self.slave_files[r].write(json.dumps(rec) + '\n')
                    return log
                self.slaves[role] = FakeSlavePlus('s%d' % role, port, log=make_log(role))
                await self.slaves[role].start()
            await self._spawn_runtime()
            self.proxy = RecordingProxy(PROXY_PORT, RT_PORT, self.p('wire.jsonl'))
            await self.proxy.start()
        reg_text = json.dumps({'version': 1, 'targets': self.targets}, indent=1, sort_keys=True)
        with open(self.p('registry.json'), 'w') as f:
            f.write(reg_text)
        self.registry_sha = sha256_file(self.p('registry.json'))
        secret = os.urandom(32)
        with open(self.p('keys.json'), 'w') as f:
            json.dump({'k1': {'secret_hex': secret.hex(), 'not_before_ms': 0, 'not_after_ms': now_ms() + 86400000}}, f)
        os.chmod(self.p('keys.json'), 0o600)
        self.secret = secret
        self.standin = Standin(self.p('requests.jsonl'), secret)
        if not self.stub:
            self.sampler = Sampler(RT_PORT, self.p('samples.jsonl'))
            self.sampler.start()
        await self.start_bridge(hooks, env)

    async def _spawn_runtime(self):
        self.runtime_n += 1
        self.rtlog = open(self.p('runtime-%d.log' % self.runtime_n), 'wb')
        # The runtime is deliberately SIGSTOPped. Some macOS/Python versions
        # report that stop to asyncio's exit watcher, which then blocks the
        # event loop in waitpid. Keep this child outside that watcher.
        self.proc = subprocess.Popen(
            [self.bin, '--project-dir', self.p('project'), '--bind', '127.0.0.1:%d' % RT_PORT,
             '--state-dir', self.p('state')], stdout=self.rtlog, stderr=self.rtlog, env=dict(os.environ, RUST_LOG='info'))
        if not await self._wait_runtime():
            raise RuntimeError('runtime not healthy within 20 s')

    async def _wait_runtime_exit(self, timeout=None):
        return await asyncio.get_running_loop().run_in_executor(None, self.proc.wait, timeout)

    async def kill_runtime(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()                          # only ever our own child
            await self._wait_runtime_exit()

    async def restart_runtime(self):
        await self.kill_runtime()
        await self._spawn_runtime()

    def signal_runtime(self, sig):
        self.proc.send_signal(sig)                   # our own child only

    async def _wait_runtime(self):
        end = time.time() + 20
        while time.time() < end:
            if self.proc.poll() is not None:
                return False
            st, body = await self.rt('GET', '/health', timeout=1.0)
            if st == 200 and isinstance(body, dict) and body.get('devices') and all(d.get('healthy') for d in body['devices']):
                return True
            await asyncio.sleep(0.2)
        return False

    def bridge_cmd(self, registry=None, registry_sha=None, journal=None, hooks=None):
        port = STUB_PORT if self.stub else PROXY_PORT
        cmd = [sys.executable, '-I', self.bridge_run, '--registry', registry or self.p('registry.json'),
               '--registry-sha256', registry_sha or self.registry_sha, '--keys', self.p('keys.json'),
               '--runtime', '127.0.0.1:%d' % port, '--journal', journal or self.p('journal.jsonl'),
               '--requests', self.p('requests.jsonl'), '--trace', self.p('trace.jsonl'),
               '--project-dir', self.p('project'), '--poll-ms', '100', '--reconcile-recheck-ms', '2000'] + self.bridge_args
        if hooks:
            cmd += ['--test-hooks', hooks]
        return cmd

    async def start_bridge(self, hooks=None, env=None):
        self.bridge_n += 1
        t_spawn = time.time()
        self.bridge_err = open(self.p('bridge-%d.stderr' % self.bridge_n), 'wb')
        self.bridge = await asyncio.create_subprocess_exec(
            *self.bridge_cmd(hooks=hooks), stdout=asyncio.subprocess.PIPE, stderr=self.bridge_err,
            env=dict(os.environ, PYTHONDONTWRITEBYTECODE='1', **(env or {})))
        try:
            line = await asyncio.wait_for(self.bridge.stdout.readline(), 15)
        except asyncio.TimeoutError:
            line = b''
        with open(self.p('bridge-%d.stdout' % self.bridge_n), 'wb') as f:
            f.write(line)
        if not line or self.bridge.returncode not in (None,):
            raise RuntimeError('bridge did not start; see bridge-%d.stderr' % self.bridge_n)
        self.startup_windows.append((t_spawn, time.time()))
        return json.loads(line.decode())

    def run_bridge_once(self, registry_text, tag):
        """Start a throw-away bridge on its own registry and journal; return (exit code, stderr).  For startup refusals."""
        reg = self.p('registry-%s.json' % tag)
        with open(reg, 'w') as f:
            f.write(registry_text)
        r = subprocess.run(self.bridge_cmd(registry=reg, registry_sha=sha256_file(reg), journal=self.p('journal-%s.jsonl' % tag)),
                           capture_output=True, text=True, timeout=20, env=dict(os.environ, PYTHONDONTWRITEBYTECODE='1'))
        return r.returncode, r.stderr

    async def stop_bridge(self):
        if self.bridge and self.bridge.returncode is None:
            self.bridge.terminate()
            try:
                await asyncio.wait_for(self.bridge.wait(), 5)
            except asyncio.TimeoutError:
                self.bridge.kill()
                await self.bridge.wait()

    async def kill_bridge(self):
        if self.bridge and self.bridge.returncode is None:
            self.bridge.kill()                        # SIGKILL: no chance to clean up
            await self.bridge.wait()

    async def stop(self):
        await self.stop_bridge()
        if self.sampler:
            self.sampler.stop()
        if self.proc and self.proc.poll() is None:
            try:
                self.signal_runtime(signal.SIGCONT)
                await self.rt('POST', '/stop', {}, timeout=3.0)
                await self._wait_runtime_exit(timeout=25)
            except Exception:  # noqa: BLE001
                self.proc.kill()                      # only ever our own child
                await self._wait_runtime_exit()
        if self.proxy:
            await self.proxy.stop()
        if self.stub_srv:
            await self.stub_srv.stop()
        for slave in self.slaves.values():
            await slave.stop()
        for f in list(self.slave_files.values()) + [getattr(self, 'rtlog', None), getattr(self, 'bridge_err', None)]:
            if f:
                f.close()

    # ---- helpers ----------------------------------------------------------------------------------------------------------
    def _rt_sync(self, method, path, body, timeout):
        conn = http.client.HTTPConnection('127.0.0.1', RT_PORT, timeout=timeout)
        try:
            data = None if body is None else json.dumps(body).encode()
            conn.request(method, path, body=data, headers={'X-IA2-Origin': 'lab-harness',
                                                           'Content-Type': 'application/json'} if data else {'X-IA2-Origin': 'lab-harness'})
            resp = conn.getresponse()
            text = resp.read().decode('utf-8', 'replace')
            try:
                return resp.status, json.loads(text)
            except ValueError:
                return resp.status, text
        except Exception as exc:  # noqa: BLE001
            return None, repr(exc)
        finally:
            conn.close()

    async def rt(self, method, path, body=None, timeout=5.0):
        return await asyncio.get_running_loop().run_in_executor(None, self._rt_sync, method, path, body, timeout)

    def trace(self):
        return read_trace(self.p('trace.jsonl'))

    def samples(self):
        return read_jsonl(self.p('samples.jsonl'))

    def wire(self):
        """What the bridge sent to IA2 (or to the stub) while serving requests: the recording proxy's log, without the reads a bridge
        makes between being spawned and printing `ready` (preflight, crash recovery)."""
        reqs = ([dict(r, ev='req') for r in self.stub_srv.requests] if self.stub
                else [r for r in read_jsonl(self.p('wire.jsonl')) if r.get('ev') == 'req'])
        return [r for r in reqs if not any(a <= r['t'] <= b for a, b in self.startup_windows)]

    def wire_all(self):
        return read_jsonl(self.p('wire.jsonl'))

    def journal(self):
        return read_jsonl(self.p('journal.jsonl'))

    def event(self, action_id):
        evs = [e for e in self.trace() if e.get('action_id') == action_id and e.get('kind') == 'action_status']
        return evs[-1] if evs else None

    def entry(self, key):
        return next(t for t in self.targets if t['key'] == key)

    def slave_writes(self, addr, after_t=0.0, slave=1):
        return [e for e in self.slave_events[slave] if e.get('ev') == 'req' and e.get('action') == 'resp'
                and e.get('fc') in (6, 16) and e.get('addr') == addr and e.get('t_req', 0) > after_t]

    async def snapshot(self, tag):
        out = {}
        for name in ('health', 'status', 'audit'):
            st, body = await self.rt('GET', '/' + name)
            out[name] = {'status': st, 'body': body}
        with open(self.p('%s.json' % tag), 'w') as f:
            json.dump(out, f, indent=1, sort_keys=True)
        return out

    def var_row(self, status_body, name):
        rows = (status_body or {}).get('last_snapshot', {}).get('vars', [])
        return next((v for v in rows if v.get('name') == name), None)


ALLOWED_WIRE = {('GET', '/status'), ('GET', '/health'), ('GET', '/audit'), ('POST', '/write')}


def wire_summary(reqs):
    return {'count': len(reqs), 'posts': [r['path'] for r in reqs if r['method'] == 'POST'],
            'outside_allowed': sorted({(r['method'], r['path']) for r in reqs} - ALLOWED_WIRE)}


# ====================================================================================================================
async def case_A1(st, ck, notes):
    await asyncio.sleep(1.0)
    st_a, audit0 = await st.rt('GET', '/audit')
    ck('IA2 audit is empty before the request', audit0 == [], audit0)
    t_submit = time.time()
    req = st.standin.submit('flow_setpoint', 12.5)
    aid = req['action_id']
    label = 'tier0.lab.%s' % aid[:8]
    ev = await wait_until(lambda: st.event(aid), 10)
    ck('the bridge answered within 10 s', ev is not None)
    if ev is None:
        return
    ck('status accepted, no reason', (ev['status'], ev.get('reason')) == ('accepted', None), (ev['status'], ev.get('reason')))
    ck('applied bits equal the request (REAL 12.5 = 1095237632)', ev.get('applied_bits') == 1095237632, ev.get('applied_bits'))
    ck('the answer carries claim and not_claimed', bool(ev.get('claim')) and 'not_claimed' in ev)
    ck('label is tier0.<agent>.<id8>', ev.get('label') == label, ev.get('label'))
    res = None
    for _ in range(80):
        res = verifier.derive(req, st.entry('flow_setpoint'), st.trace(), st.samples(), now_ms())
        if res['status'] != 'accepted':
            break
        await asyncio.sleep(0.2)
    notes['verifier_result'] = res
    ck('the independent verifier derives `verified`', res['status'] == 'verified', res)
    refs = res.get('observation_refs') or {}
    ck('the proof is the Input-mapped feedback variable', refs.get('variable') == 'sp_flow_fb', refs)
    ck('the proof is ordered after the write IN THE RUNTIME\'S SCAN CLOCK (sample scan time > the bridge\'s post-write marker)',
       isinstance(refs.get('scan_timestamp_us'), int) and refs['scan_timestamp_us'] > ev['post_scan_us'],
       (refs.get('scan_timestamp_us'), ev.get('post_scan_us')))
    ck('...and the bridge recorded the offset between its wall clock and that scan clock', isinstance(ev.get('scan_anchor_ms'), int), ev.get('scan_anchor_ms'))
    ck('the verifier accepts the bridge\'s registry hash and refuses another one',
       verifier.derive(req, st.entry('flow_setpoint'), st.trace(), st.samples(), now_ms(), registry_sha256=st.registry_sha)['status'] == 'verified'
       and verifier.derive(req, st.entry('flow_setpoint'), st.trace(), st.samples(), now_ms(), registry_sha256='0' * 64)['status'] == 'unknown')
    later = verifier.derive(req, st.entry('flow_setpoint'), st.trace(), st.samples(), req['deadline_ms'] + 1000)
    ck('the verdict is stable after the deadline', later['status'] == 'verified', later)
    ck('no `verified` anywhere in the bridge trace',
       all(e.get('status') != 'verified' for e in st.trace()), [e.get('status') for e in st.trace()])
    kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
    ck('journal order is intent, response, post_check, terminal', kinds == ['intent', 'response', 'post_check', 'terminal'], kinds)
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies', ok, n)
    reqs = st.wire()
    w = wire_summary(reqs)
    ck('exactly one POST /write reached IA2', w['posts'] == ['/write'], w)
    ck('only the expected routes were used', not w['outside_allowed'], w)
    post = next(r for r in reqs if r['method'] == 'POST')
    ck('POST body is {name, value} with the bit pattern', json.loads(post['body']) == {'name': 'sp_flow', 'value': 1095237632}, post['body'])
    ck('POST carries the label', post['origin'] == label, post['origin'])
    sa, audit = await st.rt('GET', '/audit')
    ck('IA2 /audit has exactly one entry', isinstance(audit, list) and len(audit) == 1, audit)
    if isinstance(audit, list) and audit:
        e = audit[0]
        ck('audit entry matches (origin, name, requested = applied, ok)',
           (e.get('origin'), e.get('name'), e.get('requested'), e.get('applied'), e.get('result')) == (label, 'sp_flow', 1095237632, 1095237632, 'ok'), e)
    writes = [e for e in st.slave_writes(100, t_submit) if e.get('value') == 0x4148]
    ck('the device received the REAL once (words 0x4148 0x0000)', len(writes) == 1 and st.slave.regs.get(100) == 0x4148 and st.slave.regs.get(101) == 0,
       (len(writes), st.slave.regs.get(100), st.slave.regs.get(101)))
    pc = next(r for r in st.journal() if r.get('action_id') == aid and r['kind'] == 'post_check')
    ck('post-check found the audit entry', (pc.get('audit_match') or {}).get('found') is True, pc.get('audit_match'))
    notes['timing_ms'] = {'accepted_after_submit': ev['observed_at_ms'] - req['requested_at_ms'],
                          'verified_after_sent': None if not refs else refs['observed_at_ms'] - ev['sent_at_ms']}


async def case_B1(st, ck, notes):
    await asyncio.sleep(1.0)
    expected = []

    def send(label, key, value, reason, detail=None, **override):
        p = st.standin.submit(key, value, **override)
        expected.append((label, p['action_id'], reason, detail))
    send('unknown key', 'no_such_key', 12.5, 'not_in_registry')
    send('IA2 variable name as the key', 'sp_flow', 12.5, 'not_in_registry')
    send('instance-qualified IA2 name', 'main_inst.sp_flow', 12.5, 'not_in_registry')
    for cmd in ('force', 'unforce', 'stop', 'pause', 'resume', 'inject', 'alarm_ack', 'pulse'):
        send('command %s' % cmd, 'flow_setpoint', 12.5, 'bad_request', 'unsupported_command', command=cmd)
    send('pulse_ms field', 'flow_setpoint', 12.5, 'bad_request', None, pulse_ms=100)
    send('simulation mismatch', 'flow_setpoint', 12.5, 'simulation_mismatch', simulation=False)
    ids = [e[1] for e in expected]
    await wait_until(lambda: all(st.event(i) for i in ids), 15)
    for label, aid, reason, detail in expected:
        ev = st.event(aid)
        ck('%s -> rejected/%s' % (label, reason), ev is not None and (ev['status'], ev.get('reason')) == ('rejected', reason),
           None if ev is None else (ev['status'], ev.get('reason')))
        if detail and ev:
            ck('%s detail' % label, ev.get('detail') == detail, ev.get('detail'))
    await asyncio.sleep(1.5)
    reqs = st.wire()
    ck('NOTHING reached IA2 (wire log is empty)', len(reqs) == 0, wire_summary(reqs))
    sa, audit = await st.rt('GET', '/audit')
    ck('IA2 /audit is empty', audit == [], audit)
    ck('the device never received a non-boot write', all(e.get('value') in (0, None) for e in st.slave_writes(100)), [e.get('value') for e in st.slave_writes(100)])
    ck('journal records one gate_reject per request', len([r for r in st.journal() if r['kind'] == 'gate_reject']) == len(expected))
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies', ok, n)
    ck('the bridge is still running', st.bridge.returncode is None)


async def case_B3(st, ck, notes):
    await asyncio.sleep(1.0)
    sb, before = await st.rt('GET', '/status')
    row0 = st.var_row(before, 'unlisted_sp')
    req = st.standin.submit('unlisted', 5.0)
    aid = req['action_id']
    ev = await wait_until(lambda: st.event(aid), 10)
    ck('the bridge answered within 10 s', ev is not None)
    if ev is None:
        return
    ck('status rejected / governance_denied', (ev['status'], ev.get('reason')) == ('rejected', 'governance_denied'), (ev['status'], ev.get('reason')))
    await asyncio.sleep(2.0)                                    # long enough for any (wrong) retry
    reqs = st.wire()
    w = wire_summary(reqs)
    ck('exactly one POST /write (no retry)', w['posts'] == ['/write'], w)
    resp = [r for r in st.wire_all() if r.get('ev') == 'resp']
    statuses = [r['status'] for r in resp]
    ck('IA2 answered the write with 403', 403 in statuses, statuses)
    sa, audit = await st.rt('GET', '/audit')
    ck('IA2 /audit has one entry, a denial without `applied`',
       isinstance(audit, list) and len(audit) == 1 and 'applied' not in audit[0] and 'governance' in str(audit[0].get('result')), audit)
    sc, after = await st.rt('GET', '/status')
    row1 = st.var_row(after, 'unlisted_sp')
    ck('the variable did not change', row0 is not None and row1 is not None and row0['bits'] == row1['bits'] == 0, (row0, row1))
    ck('the device received nothing at 140 but the boot value', all(e.get('value') in (0, None) for e in st.slave_writes(140)), [e.get('value') for e in st.slave_writes(140)])
    kinds = [r['kind'] for r in st.journal() if r.get('action_id') == aid]
    ck('journal: intent, response, post_check, terminal', kinds == ['intent', 'response', 'post_check', 'terminal'], kinds)
    ck('no `disable` record (403 is not an unknown variable)', not any(r['kind'] == 'disable' for r in st.journal()))
    ck('claim says "未写入"', ev.get('claim') == '未写入', ev.get('claim'))


async def case_C1(st, ck, notes):
    await asyncio.sleep(1.0)
    sc, resp = await st.rt('POST', '/inject-scan-stall', {'stall_ms': 100, 'scans': 10})
    notes['inject_response'] = [sc, resp]

    async def tripped():
        s, h = await st.rt('GET', '/health', timeout=1.0)
        return isinstance(h, dict) and h.get('watchdog_tripped') is True
    end = time.time() + 10
    while time.time() < end and not await tripped():
        await asyncio.sleep(0.1)
    snap = await st.snapshot('latched-before-a')
    ck('the runtime latched (watchdog_tripped = true)', snap['health']['body'].get('watchdog_tripped') is True, snap['health']['body'])
    notes['latched_health'] = snap['health']['body']
    wire0 = len(st.wire())
    audit_n0 = len(snap['audit']['body'] or [])
    t_a = time.time()
    # ---- (a) normal bridge -------------------------------------------------------------------------------------------
    reqa = st.standin.submit('flow_setpoint', 12.5)
    eva = await wait_until(lambda: st.event(reqa['action_id']), 10)
    ck('(a) the bridge answered', eva is not None)
    if eva:
        ck('(a) rejected / latched', (eva['status'], eva.get('reason')) == ('rejected', 'latched'), (eva['status'], eva.get('reason')))
    await asyncio.sleep(1.0)
    new = st.wire()[wire0:]
    ck('(a) no POST reached IA2', not [r for r in new if r['method'] == 'POST'], wire_summary(new))
    sa, audit = await st.rt('GET', '/audit')
    ck('(a) IA2 /audit gained no entry', len(audit) == audit_n0, (audit_n0, len(audit)))
    ck('(a) the device did not receive 12.5', not [e for e in st.slave_writes(100, t_a) if e.get('value') == 0x4148])
    # ---- (b) diagnostic bridge: gate G8 off ------------------------------------------------------------------------
    await st.stop_bridge()
    info = await st.start_bridge(HOOKS_SKIP_G8)
    notes['bridge2_start'] = info
    t_b = time.time()
    sb0, status0 = await st.rt('GET', '/status')
    row0 = st.var_row(status0, 'sp_flow')
    reqb = st.standin.submit('flow_setpoint', 13.5)
    evb = await wait_until(lambda: st.event(reqb['action_id']), 15)
    ck('(b) the diagnostic bridge answered', evb is not None)
    await asyncio.sleep(1.5)
    sb1, status1 = await st.rt('GET', '/status')
    row1 = st.var_row(status1, 'sp_flow')
    snap = await st.snapshot('latched-after-b')
    resp_rec = next((r for r in st.journal() if r.get('action_id') == reqb['action_id'] and r['kind'] == 'response'), None)
    http_view = (resp_rec or {}).get('http', {})
    try:
        body = json.loads(http_view.get('text') or 'null')
    except ValueError:
        body = None
    received = [e for e in st.slave_writes(100, t_b) if e.get('value') == 0x4158]       # first word of REAL 13.5
    ch = {'trace_status': None if evb is None else (evb['status'], evb.get('reason')),
          'ia2_http_status': http_view.get('status'), 'ia2_body': body,
          'sp_flow_before': row0, 'sp_flow_after': row1,
          'device_received_13_5': len(received), 'device_writes_at_100_after_b': [hex(e.get('value') or 0) for e in st.slave_writes(100, t_b)],
          'audit_after': snap['audit']['body']}
    notes['characterization_C1b'] = ch
    f2 = (http_view.get('status') == 200 and isinstance(body, dict) and body.get('undelivered_device') is None and not received)
    notes['F2_reproduced_on_1c42df7'] = f2
    ck('(b) the bridge classifies the write as unknown / latched_after_write (never accepted)',
       evb is not None and evb['status'] == 'unknown' and evb.get('reason') == 'latched_after_write', ch['trace_status'])
    ck('(b) the diagnostic bridge says so in its journal', any(r['kind'] == 'start' and r.get('hooks') for r in st.journal()))
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies across the bridge restart', ok, n)
    notes['finding'] = None if f2 else 'C1(b): IA2 did not behave as EXPECT (200 + undelivered_device null + nothing reaches the device); see characterization_C1b'


CASES = {'A1': (case_A1, [FLOW]), 'B1': (case_B1, [FLOW]), 'B3': (case_B3, [UNLISTED], {'bridge_args': ['--governance-check', 'warn']}),
         'C1': (case_C1, [FLOW])}


def tree_hash(paths):
    h = hashlib.sha256()
    for p in sorted(paths):
        h.update(p.encode())
        h.update(bytes.fromhex(sha256_file(os.path.join(ROOT, p))))
    return h.hexdigest()


def code_files():
    out = []
    for base in ('bridge', 'lab'):
        for directory, dirs, names in os.walk(os.path.join(ROOT, base)):
            dirs[:] = sorted(d for d in dirs if d != '__pycache__')
            for name in sorted(names):
                if name.endswith('.py'):
                    out.append(os.path.relpath(os.path.join(directory, name), ROOT).replace(os.sep, '/'))
    return out + ['bridge_run.py', 'contract_vocab.json']


async def run_case(case_id, args):
    n = 1
    while os.path.exists(os.path.join(args.out, '%s-r%d' % (case_id, n))):
        n += 1
    case_dir = os.path.join(args.out, '%s-r%d' % (case_id, n))
    entry = CASES[case_id]
    fn, targets = entry[0], entry[1]
    opts = entry[2] if len(entry) > 2 else {}
    st = Stack(case_dir, args.bin, targets, stub=opts.get('stub', False), bridge_args=opts.get('bridge_args'), prod=opts.get('prod', False),
               project_opts=opts.get('project'))
    ck, notes = Checks(), {}
    started = time.time()
    error = None
    try:
        await st.start(hooks=opts.get('hooks'), env={k: v.format(dir=case_dir) for k, v in (opts.get('env') or {}).items()} or None)
        await fn(st, ck, notes)
        if not st.stub:
            await st.snapshot('final')
    except Exception as exc:  # noqa: BLE001
        import traceback
        error = traceback.format_exc()
    finally:
        try:
            await st.stop()
        except Exception as exc:  # noqa: BLE001
            error = (error or '') + '\nstop failed: %r' % exc
    # universal: the signing secret must not appear anywhere in the evidence (keys.json itself excepted)
    leaked = []
    if hasattr(st, 'secret'):
        for base, _, files in os.walk(case_dir):
            for name in files:
                if name in ('keys.json', 'MANIFEST.sha256'):
                    continue
                with open(os.path.join(base, name), 'rb') as f:
                    data = f.read()
                if st.secret.hex().encode() in data or st.secret in data:
                    leaked.append(os.path.relpath(os.path.join(base, name), case_dir))
    ck('no signing-secret material anywhere in the evidence', not leaked, leaked)
    verdict = 'FAIL' if (error or ck.failed) else ('FINDING' if notes.get('finding') else 'PASS')
    out = {'case': case_id, 'run': n, 'verdict': verdict, 'error': error, 'checks': ck.items, 'notes': notes,
           'started': started, 'seconds': round(time.time() - started, 1), 'runtime_bin': args.bin,
           'runtime_bin_sha256': sha256_file(args.bin), 'python': sys.version.split()[0], 'platform': platform.platform(),
           'code_tree_sha256': tree_hash(code_files()), 'code_files': code_files(),
           'registry_sha256': getattr(st, 'registry_sha', None)}
    with open(os.path.join(case_dir, 'verdict.json'), 'w') as f:
        json.dump(out, f, indent=1, sort_keys=True)
    lines = subprocess.run(['/bin/sh', '-c', 'cd "%s" && find . -type f ! -name MANIFEST.sha256 -print0 | sort -z | xargs -0 shasum -a 256' % case_dir],
                           capture_output=True, text=True).stdout
    with open(os.path.join(case_dir, 'MANIFEST.sha256'), 'w') as f:
        f.write(lines)
    return out


async def main_async(args):
    os.makedirs(args.out, exist_ok=True)
    results = []
    for case_id in args.cases.split(','):
        r = await run_case(case_id, args)
        results.append(r)
        print('%-4s %-8s checks=%d failed=%d (%.1fs)%s' % (case_id, r['verdict'], len(r['checks']), len([c for c in r['checks'] if not c['ok']]),
                                                          r['seconds'], '  ERROR' if r['error'] else ''), flush=True)
        for c in r['checks']:
            if not c['ok']:
                print('     FAILED: %s -- %s' % (c['name'], c['detail']), flush=True)
        if r['error']:
            print(r['error'], flush=True)
    return results


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--bin', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--cases', default='A1,B1,B3,C1')
    args = ap.parse_args()
    args.out = os.path.abspath(args.out)
    results = asyncio.run(main_async(args))
    if any(r['verdict'] == 'FAIL' or r['error'] for r in results):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
