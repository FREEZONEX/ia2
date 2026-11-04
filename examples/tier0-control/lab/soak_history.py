"""Real 5 Hz history-budget / whole-session rotation acceptance (loopback only).

Choose a fresh --out; --seconds 4500 crosses the unmodified 20k sample limit.
Use --rotate-after 2100 for a second lane. A short pilot exercises rotation but
does not prove the real elapsed-time capacity boundary. No fault injection.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import secrets
import sys
import time
import traceback

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import harness
from cases_p0 import FLOW


def save(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + '\n')


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Counter:
    def __init__(self, path):
        self.path, self.offset, self.count = Path(path), 0, 0

    def read(self):
        if self.path.exists():
            with self.path.open('rb') as f:
                f.seek(self.offset)
                while line := f.readline():
                    if not line.endswith(b'\n'):
                        break
                    self.count += 1
                    self.offset = f.tell()
        return self.count


class Session:
    def __init__(self, stack, root):
        self.st, self.root = stack, Path(root)
        self.seq, self.starts, self.actions = 0, 0, []
        self.mcp = self.bridge = self.sampler = None
        self.handles = []
        self.counter = Counter(self.root / 'samples.jsonl')

    def log_handle(self, name):
        f = (self.root / name).open('ab', buffering=0)
        self.handles.append(f)
        return f

    def common(self):
        return ['--registry', str(self.root / 'registry.json'), '--registry-sha256', self.st.registry_sha,
                '--keys', str(self.root / 'keys.json'), '--event-keys', str(self.root / 'event-keys.json'),
                '--requests', str(self.root / 'requests.jsonl'), '--trace', str(self.root / 'trace.jsonl')]

    async def start(self):
        self.root.mkdir(mode=0o700)
        (self.root / 'registry.json').write_bytes(Path(self.st.p('registry.json')).read_bytes())
        now = int(time.time() * 1000)
        for role, filename in [('request', 'keys.json'), ('event', 'event-keys.json'), ('sample', 'sample-keys.json')]:
            path = self.root / filename
            fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            with os.fdopen(fd, 'w') as stream:
                json.dump({role + '-' + secrets.token_hex(8): dict(secret_hex=secrets.token_hex(32),
                          not_before_ms=now-60000, not_after_ms=now+86400000)}, stream)
        package = self.st.p('prod-package')
        bridge = [sys.executable, '-I', '-B', package + '/bridge_run.py', *self.common(),
                  '--runtime', '127.0.0.1:%d' % harness.PROXY_PORT,
                  '--journal', str(self.root / 'journal.jsonl'), '--project-dir', self.st.p('project'),
                  '--poll-ms', '100']
        sampler = [sys.executable, '-I', '-B', package + '/sampler_run.py',
                   '--runtime', '127.0.0.1:%d' % harness.RT_PORT,
                   '--samples', str(self.root / 'samples.jsonl'), '--sample-keys', str(self.root / 'sample-keys.json')]
        save(self.root / 'commands.json', dict(bridge=bridge, sampler=sampler, interval_ms='default 200'))
        for name, cmd in [('bridge', bridge), ('sampler', sampler)]:
            proc = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE,
                                                        stderr=self.log_handle(name + '.stderr'))
            setattr(self, name, proc)
            raw = await asyncio.wait_for(proc.stdout.readline(), 20)
            self.log_handle(name + '.stdout').write(raw)
            assert json.loads(raw).get('ready') is True, (name, raw)
        self.transcript = self.log_handle('mcp-transcript.jsonl')
        await self.start_mcp()

    async def start_mcp(self):
        self.starts += 1
        self.mcp = await asyncio.create_subprocess_exec(sys.executable, '-I', '-B',
            self.st.p('prod-package/control_mcp_run.py'), *self.common(),
            '--samples', str(self.root / 'samples.jsonl'), '--sample-keys', str(self.root / 'sample-keys.json'),
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=self.log_handle('mcp.stderr'))
        await self.send('initialize', dict(protocolVersion='2025-11-25', capabilities={},
                                         clientInfo=dict(name='history-acceptance', version='1')))
        self.mcp.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        await self.mcp.stdin.drain()

    async def send(self, method, params):
        self.seq += 1
        message = dict(jsonrpc='2.0', id=self.seq, method=method, params=params)
        started = time.monotonic()
        self.mcp.stdin.write(json.dumps(message).encode() + b'\n')
        await self.mcp.stdin.drain()
        raw = await asyncio.wait_for(self.mcp.stdout.readline(), 30)
        reply = json.loads(raw)
        self.transcript.write((json.dumps(dict(wall=time.time(), mono=started, request=message,
            response=reply, mcp_start=self.starts, elapsed_ms=(time.monotonic()-started)*1000))+'\n').encode())
        assert reply.get('id') == self.seq and 'error' not in reply, reply
        return reply['result']

    async def call(self, name, **args):
        result = await self.send('tools/call', dict(name=name, arguments=args))
        return result.get('isError', False), json.loads(result['content'][0]['text'])

    async def submit_verified(self):
        value = 12.5 if len(self.actions) % 2 == 0 else 13.5
        error, requested = await self.call('ia2_set', target_key='flow_setpoint', target=value)
        assert not error and requested['status'] == 'requested', requested
        aid = requested['action_id']
        self.actions.append(aid)
        end = time.monotonic() + 15
        while time.monotonic() < end:
            error, status = await self.call('get_action_status', action_id=aid)
            assert not error and status['rejected_envelopes'] == 0, status
            if status['status'] == 'verified':
                return aid
            assert status['status'] in ('requested', 'accepted'), status
            await asyncio.sleep(0.5)
        raise AssertionError(('action did not verify', aid, status))

    async def stop_mcp(self):
        if self.mcp and self.mcp.returncode is None:
            self.mcp.stdin.close()
            try:
                await asyncio.wait_for(self.mcp.wait(), 5)
            except asyncio.TimeoutError:
                self.mcp.kill()
                await self.mcp.wait()
            assert self.mcp.returncode == 0, self.mcp.returncode

    async def stop(self):
        for proc in (self.bridge, self.sampler):
            if proc and proc.returncode is None:
                proc.terminate()
                try:
                    await asyncio.wait_for(proc.wait(), 5)
                except asyncio.TimeoutError:
                    proc.kill()
                    await proc.wait()
        await self.stop_mcp()
        for f in self.handles:
            f.close()

    def seal(self):
        files = {str(p.relative_to(self.root)): dict(bytes=p.stat().st_size, sha256=digest(p))
                 for p in sorted(self.root.rglob('*')) if p.is_file()}
        save(self.root.parent / (self.root.name + '-manifest.json'), files)
        return files


async def run(args):
    root = Path(args.out).resolve()
    root.mkdir(parents=True, exist_ok=False)
    st = harness.Stack(str(root/'runtime'), os.path.abspath(args.bin), [FLOW], no_bridge=True, prod=True)
    sessions, sealed, error, overflow, archive_task = [], [], None, None, None
    started = None
    audit_file = (root/'audit-archive.jsonl').open('w', buffering=1)
    async def archive():
        while True:
            code, body = await st.rt('GET', '/audit')
            assert code == 200, (code, body)
            audit_file.write(json.dumps(dict(wall=time.time(), entries=body))+'\n')
            await asyncio.sleep(1)
    try:
        await st.start()
        _, initial = await st.rt('GET', '/status')
        save(root/'initial.json', initial)
        archive_task = asyncio.create_task(archive())
        session = Session(st, root/'session-01')
        sessions.append(session)
        await session.start()
        first = await session.submit_verified()
        started = time.monotonic()
        next_request = started + args.request_every
        next_progress = started
        rotated = False
        while time.monotonic()-started < args.seconds:
            elapsed = time.monotonic()-started
            assert not archive_task.done(), 'audit archiver stopped'
            assert st.proc.poll() is None and all(p.returncode is None for p in (session.mcp, session.bridge, session.sampler))
            count = session.counter.read()
            if args.rotate_after and not rotated and elapsed >= args.rotate_after:
                # No concurrent submits: every submitted action completed its
                # verification above. Re-query all before closing this session.
                for aid in session.actions:
                    bad, result = await session.call('get_action_status', action_id=aid)
                    assert not bad and result['status'] == 'verified', result
                await session.stop()
                sealed.append((session.root, session.seal()))
                old = session
                session = Session(st, root/'session-02')
                sessions.append(session)
                await session.start()
                bad, result = await session.call('get_action_status', action_id=first)
                assert bad and 'unknown or conflicting' in result['error']['message'], result
                _, current = await st.rt('GET', '/status')
                assert current['runtime_id'] == initial['runtime_id']
                save(root/'rotation.json', dict(elapsed=elapsed, old_actions=len(old.actions),
                    old_samples=old.counter.read(), runtime_id=current['runtime_id'], old_id_in_new_session=result,
                    pending_actions=0, replayed_actions=0))
                await session.submit_verified()
                next_request = time.monotonic()+args.request_every
                rotated = True
            elif count > 20000 and overflow is None:
                before = digest(session.root/'requests.jsonl')
                replies = []
                for restart in (False, True):
                    if restart:
                        await session.stop_mcp()
                        await session.start_mcp()
                    for name, arguments in [('get_action_status', dict(action_id=first)),
                                            ('ia2_set', dict(target_key='flow_setpoint', target=14.5))]:
                        bad, result = await session.call(name, **arguments)
                        assert bad and 'record limit' in result['error']['message'], result
                        replies.append(dict(restarted=restart, tool=name, result=result))
                assert digest(session.root/'requests.jsonl') == before
                overflow = dict(samples=count, elapsed=elapsed, replies=replies, request_sha256=before)
                save(root/'overflow.json', overflow)
            elif time.monotonic() >= next_request and count < 19500:
                await session.submit_verified()
                next_request = time.monotonic()+args.request_every
            if time.monotonic() >= next_progress:
                progress = dict(elapsed_seconds=round(time.monotonic()-started, 3), session=session.root.name,
                                samples=session.counter.read(), actions=sum(len(s.actions) for s in sessions),
                                overflow_checked=overflow is not None, rotated=rotated)
                save(root/'progress.json', progress)
                print(json.dumps(progress), flush=True)
                next_progress = time.monotonic()+60
            await asyncio.sleep(0.25)
        duration = time.monotonic()-started
        assert duration >= args.seconds
        if args.rotate_after:
            assert rotated and overflow is None and all(s.counter.read()<20000 for s in sessions)
        elif args.expect_overflow:
            assert overflow is not None, 'did not reach real 20k capacity'
        for s in sessions:
            if s is session and not overflow:
                for aid in s.actions:
                    bad, result = await s.call('get_action_status', action_id=aid)
                    assert not bad and result['status']=='verified', result
        await st.snapshot('final')
        _, final = await st.rt('GET','/status')
        assert final['runtime_id']==initial['runtime_id'] and not final['fault'] and not final['forces']
        assert not final['watchdog_tripped']
        save(root/'final.json', final)
    except BaseException:
        error = traceback.format_exc()
    finally:
        for s in sessions:
            try:
                await s.stop()
            except Exception:
                error = (error or '')+traceback.format_exc()
        if archive_task:
            archive_task.cancel()
            await asyncio.gather(archive_task, return_exceptions=True)
        audit_file.close()
        await st.stop()
    unchanged = all(p.exists() and all((p/name).stat().st_size==v['bytes'] and digest(p/name)==v['sha256']
                    for name,v in files.items()) for p,files in sealed)
    if not unchanged:
        error = (error or '')+'sealed session changed'
    for s in sessions:
        if not any(s.root==p for p,_ in sealed):
            s.seal()
    result = dict(verdict='FAIL' if error else 'PASS', error=error, args=vars(args),
                  active_seconds=locals().get('duration'), first_session_unchanged=unchanged,
                  overflow=overflow, runtime_exit=st.proc.returncode if st.proc else None,
                  sessions=[dict(path=str(s.root), actions=s.actions, samples=s.counter.read(), mcp_starts=s.starts,
                                 sampler_exit=s.sampler.returncode if s.sampler else None,
                                 bridge_exit=s.bridge.returncode if s.bridge else None) for s in sessions])
    save(root/'summary.json', result)
    print(json.dumps(dict(verdict=result['verdict'], error=error, active_seconds=result['active_seconds'])), flush=True)
    return 0 if result['verdict']=='PASS' else 1


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--seconds', type=float, default=4500)
    parser.add_argument('--rotate-after', type=float, default=0)
    parser.add_argument('--request-every', type=float, default=30)
    parser.add_argument('--expect-overflow', action='store_true')
    sys.exit(asyncio.run(run(parser.parse_args())))
