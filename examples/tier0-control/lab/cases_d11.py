"""Signed-path adversarial cases and real stdio MCP on a sandbox runtime."""
import asyncio
import json
import os
import secrets
import sys

from harness import FLOW, ROOT, now_ms, wait_until
from readside import verifier


async def forged(st, ck, notes, mode):
    await asyncio.sleep(0.6)
    if mode in ('sample', 'both'):
        st.sampler.paused.set()
        await asyncio.sleep(0.4)
    request = st.standin.submit('flow_setpoint', 12.5)
    event = await wait_until(lambda: st.event(request['action_id']), 10)
    ck('a genuine bridge acceptance exists', event and event['status'] == 'accepted', event)
    if not event:
        return
    aid = request['action_id']
    if mode in ('event', 'both'):
        aid = secrets.token_hex(16)
        fake = dict(event, action_id=aid, event_id=secrets.token_hex(16))
        with open(st.p('trace.jsonl'), 'a') as f:
            f.write(json.dumps(fake) + '\n')
    if mode in ('sample', 'both'):
        sample = st.samples()[-1]
        sample.update(event_id=secrets.token_hex(16), collected_at_ms=now_ms(), native_runtime_id=event['runtime_id'],
                      scan_timestamp_us=event['post_scan_us'] + 1000)
        for var in sample['variables']:
            if var['name'] == 'sp_flow_fb':
                var['decoded_value'] = 12.5
        with open(st.p('samples.jsonl'), 'a') as f:
            f.write(json.dumps(sample) + '\n')
    else:
        await asyncio.sleep(0.5)
    result = verifier.derive(aid, st.entry('flow_setpoint'), st.event_envelopes(), st.sample_envelopes(), now_ms(),
                             registry_sha256=st.registry_sha, event_keys=st.event_keys, sample_keys=st.sample_keys)
    notes['verifier_result'] = result
    ck('forged input never becomes verified', result['status'] != 'verified', result)
    ck('forged records are counted', result['rejected_envelopes'] == (2 if mode == 'both' else 1), result)
    ck('only the genuine baseline request reached the runtime', len([r for r in st.wire() if r['method'] == 'POST']) == 1)


async def case_F6(st, ck, notes):
    await forged(st, ck, notes, 'event')


async def case_F7(st, ck, notes):
    await forged(st, ck, notes, 'sample')


async def case_F8(st, ck, notes):
    await forged(st, ck, notes, 'both')


async def case_MCP(st, ck, notes):
    launcher = st.p('prod-package/control_mcp_run.py') if st.prod else os.path.join(ROOT, 'control_mcp_run.py')
    args = [sys.executable, '-I', '-B', launcher, '--registry', st.p('registry.json'), '--registry-sha256', st.registry_sha,
            '--keys', st.p('keys.json'), '--event-keys', st.p('event-keys.json'), '--sample-keys', st.p('sample-keys.json'),
            '--requests', st.p('requests.jsonl'), '--trace', st.p('trace.jsonl'), '--samples', st.p('samples.jsonl')]
    transcript, procs, logs = [], [], []
    async def send(proc, method, params=None, ident=1):
        msg = dict(jsonrpc='2.0', method=method, params=params or {})
        if ident is not None:
            msg['id'] = ident
        proc.stdin.write(json.dumps(msg).encode() + b'\n')
        await proc.stdin.drain()
        if ident is None:
            return None
        raw = await asyncio.wait_for(proc.stdout.readline(), 5)
        reply = json.loads(raw)
        transcript.append(dict(request=msg, response=reply))
        return reply

    async def start():
        log = open(st.p('mcp-%d.stderr' % len(procs)), 'wb')
        logs.append(log)
        p = await asyncio.create_subprocess_exec(*args, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=log)
        procs.append(p)
        r = await send(p, 'initialize', dict(protocolVersion='2025-11-25', capabilities={}, clientInfo=dict(name='lab', version='1')))
        ck('MCP negotiated a protocol and tools capability', r['result']['protocolVersion'] == '2025-11-25' and 'tools' in r['result']['capabilities'])
        await send(p, 'notifications/initialized', ident=None)
        return p

    try:
        p = await start()
        r = await send(p, 'tools/list')
        ck('exactly the two control tools are exposed', [x['name'] for x in r['result']['tools']] == ['ia2_set', 'get_action_status'])
        r = await send(p, 'tools/call', dict(name='ia2_set', arguments=dict(target_key='flow_setpoint', target=12.5)))
        result = json.loads(r['result']['content'][0]['text'])
        aid = result['action_id']
        ck('submission reports requested, not execution success', result['status'] == 'requested')
        for _ in range(60):
            r = await send(p, 'tools/call', dict(name='get_action_status', arguments=dict(action_id=aid)))
            result = json.loads(r['result']['content'][0]['text'])
            if result['status'] not in ('requested', 'accepted'):
                break
            await asyncio.sleep(0.2)
        ck('MCP derives verified from actual signed runtime feedback', result['status'] == 'verified', result)
        notes['verifier_result'] = result
        p.stdin.close()
        await asyncio.wait_for(p.wait(), 5)
        p = await start()
        r = await send(p, 'tools/call', dict(name='get_action_status', arguments=dict(action_id=aid)))
        result = json.loads(r['result']['content'][0]['text'])
        ck('restarting MCP recovers the same action without another request', result['status'] == 'verified', result)
        ck('exactly one runtime write across MCP restart', len([r for r in st.wire() if r['method'] == 'POST']) == 1)
        r = await send(p, 'tools/call', dict(name='force', arguments={}))
        ck('unexported operation is refused', r.get('error', {}).get('code') == -32602)
    finally:
        for p in procs:
            if p.returncode is None:
                p.terminate()
                await asyncio.wait_for(p.wait(), 5)
        for log in logs:
            log.close()
        with open(st.p('mcp-transcript.json'), 'w') as f:
            json.dump(transcript, f, indent=1)


CASES = {'F6': (case_F6, [FLOW]), 'F7': (case_F7, [FLOW]), 'F8': (case_F8, [FLOW]),
         'MCP': (case_MCP, [FLOW]), 'MCPPROD': (case_MCP, [FLOW], {'prod': True})}
