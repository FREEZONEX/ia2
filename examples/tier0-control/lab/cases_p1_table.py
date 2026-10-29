"""The P1 rows of TEST-CASES.md section 3 that run end to end on the lab (exit criterion 2).  Registered like the P0 cases.

  A3   labels sent straight to the runtime: what /audit records as `origin`              (characterization)
  A4   300 writes through the bridge; /audit keeps 256; the reconciliation program says "uncheckable" for the lost ones
  B5   (a) a registry entry naming no variable: 404, rejected and disabled; (b) a write after POST /stop
  B10  simulation flag mismatch, both directions
  C2   the watchdog trips between the gates and the send: never `accepted`
  C5   (a) paused, (b) target forced: rejected; and what IA2 itself does with such a write (direct, characterization)
  C7   a required device unhealthy while /health.status is ok
  C10  northbound.toml variants: refused at start, accepted at start, and changed under a running bridge
  R4   the hold file: refused while present, across a bridge restart; an earlier write is untouched
  R5   50 requests inside a second (10 on one target) after 55 earlier writes: busy and the bridge-wide limit
  F3   one registry byte changed: the bridge refuses to start
  F4   the offline registry check flags a range outside governance and a name that is not the runtime's
  F5   an enabled entry with requires_approval: refused at start; disabled, it starts and its requests never reach IA2

E4-E7 are the verifier's table vectors (tests/test_verifier.py); C9's build (#98) is main itself.  See VALIDATION.md."""
import asyncio
import http.client
import json
import os
import stat
import subprocess
import sys
import time

import check_registry
from cases_p0 import (HOOKS_SYNC, audit_list, f32bits, fb, healthy, posts, release, short, submit_wait, tgt, vars_of, wait_marker)
from harness import ROOT, RT_PORT, sha256_file, verify_chain, wait_until, wire_summary

RECONCILE = os.path.join(ROOT, 'tools', 'reconcile_report.py')
FLOW = tgt('flow_setpoint', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], verify=fb('sp_flow_fb'))
NOFB = tgt('flow_nofb', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], single_flight_ms=0)
BIG_NOFB = tgt('big_nofb', 'big', 'DINT', -1000, 1000, require_devices=['coupler'], single_flight_ms=0)


def direct_write(name, bits, origin, path='/write'):
    """POST straight to the sandbox runtime with exactly this X-IA2-Origin (None: no header at all)."""
    conn = http.client.HTTPConnection('127.0.0.1', RT_PORT, timeout=5)
    try:
        conn.putrequest('POST', path)
        body = json.dumps({'name': name, 'value': bits}).encode()
        conn.putheader('Content-Type', 'application/json')
        conn.putheader('Content-Length', str(len(body)))
        if origin is not None:
            conn.putheader('X-IA2-Origin', origin)
        conn.endheaders(body)
        resp = conn.getresponse()
        text = resp.read().decode('utf-8', 'replace')
        try:
            return resp.status, json.loads(text)
        except ValueError:
            return resp.status, text
    finally:
        conn.close()


async def rt_direct(*args, **kw):
    return await asyncio.get_running_loop().run_in_executor(None, lambda: direct_write(*args, **kw))


async def off_loop(fn, *args):
    """Run a blocking call in a thread: the fake devices live on the harness's event loop, and a loop blocked for a second makes the
    runtime's Modbus reads time out (both devices drop at once)."""
    return await asyncio.get_running_loop().run_in_executor(None, lambda: fn(*args))


def other_bridge(st, registry_text, tag, extra=()):
    """Start a second bridge on its own registry, journal, request and trace files; return (exit code or None if it came up, its
    first stdout line, stderr).  One that comes up is terminated at once: it has nothing to read, so it sends nothing.  Blocking:
    call it through off_loop."""
    reg = st.p('registry-%s.json' % tag)
    with open(reg, 'w') as f:
        f.write(registry_text)
    cmd = st.bridge_cmd(registry=reg, registry_sha=sha256_file(reg), journal=st.p('journal-%s.jsonl' % tag))
    cmd[cmd.index('--requests') + 1] = st.p('requests-%s.jsonl' % tag)
    cmd[cmd.index('--trace') + 1] = st.p('trace-%s.jsonl' % tag)
    cmd += list(extra)
    open(st.p('requests-%s.jsonl' % tag), 'w').close()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=dict(os.environ, PYTHONDONTWRITEBYTECODE='1'))
    line = proc.stdout.readline()
    if proc.poll() is None and line:
        proc.terminate()
        _, err = proc.communicate(timeout=10)
        return None, line, err
    _, err = proc.communicate(timeout=20)
    return proc.returncode, line, err


def registry_text(targets):
    return json.dumps({'version': 1, 'targets': targets}, indent=1, sort_keys=True)


# ---- A ---------------------------------------------------------------------------------------------------------------------
async def case_A3(st, ck, notes):
    await asyncio.sleep(1.0)
    labels = ['lab.a-b_c', 'lab:colon', 'lab space', 'L' * 70, '', None]
    answers = []
    for i, label in enumerate(labels):
        answers.append(await rt_direct('sp_flow', f32bits(1.0 + i), label))
    ck('every direct write was answered 200', [a[0] for a in answers] == [200] * len(labels), answers)
    audit = await audit_list(st)
    got = [e.get('origin') for e in audit]
    expect = [l if l else 'anonymous' for l in labels]
    notes['origins'] = [{'sent': l, 'recorded': g} for l, g in zip(labels, got)]
    ck('one /audit entry per direct write', len(audit) == len(labels), got)
    ck('no entry carries an http_origin (none was sent)', all('http_origin' not in e for e in audit), audit)
    if got != expect:
        notes['finding'] = 'A3: the direct path did not record the raw non-empty header (or anonymous for none): %r' % notes['origins']
    notes['note'] = ('The bridge only ever sends [A-Za-z0-9._-]{1,64} (CONTRACT 8.1), so the characters the two paths treat differently '
                     'never occur in its labels. The IDE-server proxy path (A3b) is not exercised in 1a.')


async def archive_audit(st, path, stop):
    """Once a second, read /audit straight from the runtime, as the read-only collector does (reconcile_report's archive format)."""
    with open(path, 'w') as f:
        while not stop.is_set():
            t = time.time()
            s1, a = await st.rt('GET', '/status', timeout=2.0)
            s2, audit = await st.rt('GET', '/audit', timeout=2.0)
            s3, b = await st.rt('GET', '/status', timeout=2.0)
            rec = {'t': round(time.time(), 6), 'runtime_id': (a or {}).get('runtime_id') if isinstance(a, dict) else None,
                   'runtime_id_after': (b or {}).get('runtime_id') if isinstance(b, dict) else None}
            if s2 == 200 and isinstance(audit, list):
                rec['audit'] = audit
            f.write(json.dumps(rec, sort_keys=True, separators=(',', ':')) + '\n')
            f.flush()
            await asyncio.sleep(max(0.05, 1.0 - (time.time() - t)))


def reconcile(st, archive, tag):
    with open(st.p('device-map.json'), 'w') as f:
        json.dump({'sp_flow': {'device': '1', 'kind': 'f32', 'addr': 100}, 'big': {'device': '1', 'kind': 'i32', 'addr': 120}}, f)
    out = st.p('reconcile-%s.json' % tag)
    r = subprocess.run([sys.executable, '-I', '-B', RECONCILE, '--journal', st.p('journal.jsonl'), '--audit-archive', archive,
                        '--device-map', st.p('device-map.json'), '--device-log', '1=' + st.p('slave-1.jsonl'), '--wire', st.p('wire.jsonl'),
                        '--out', out], capture_output=True, text=True, timeout=120)
    return r.returncode, (json.load(open(out)) if os.path.exists(out) else None), r.stderr


async def case_A4(st, ck, notes):
    """300 writes through the bridge, paced under the bridge-wide 60 a minute; the archive reads /audit every second."""
    await asyncio.sleep(1.0)
    stop = asyncio.Event()
    task = asyncio.ensure_future(archive_audit(st, st.p('audit-archive.jsonl'), stop))
    ps, last = [], {}
    for i in range(300):
        key = 'flow_nofb' if i % 2 == 0 else 'big_nofb'
        value = round(1.0 + (i % 200) * 0.17, 2) if key == 'flow_nofb' else (i % 1999) - 999
        if last.get(key) == value:
            value = value + 1 if key == 'big_nofb' else value + 0.05
        last[key] = value
        ps.append(st.standin.submit(key, value, ttl_ms=60000))
        await asyncio.sleep(1.03)
    await wait_until(lambda: all(st.event(p['action_id']) for p in ps), 60)
    await asyncio.sleep(12.0)                                   # past the reconciler's coverage window for the last write
    stop.set()
    await task
    statuses = [short(st.event(p['action_id'])) for p in ps]
    notes['statuses'] = {str(s): statuses.count(s) for s in set(statuses)}
    ck('all 300 accepted', statuses.count(('accepted', None)) == 300, notes['statuses'])
    intents = [r for r in st.journal() if r['kind'] == 'intent']
    ck('the journal holds 300 intents', len(intents) == 300, len(intents))
    ok, n = verify_chain(st.p('journal.jsonl'))
    ck('journal hash chain verifies', ok, n)
    audit = await audit_list(st)
    ck('/audit now holds only the newest 256', len(audit) == 256, len(audit))
    labels = ['tier0.lab.%s' % p['action_id'][:8] for p in ps]
    ck('...the 44 oldest are gone', [e.get('origin') for e in audit] == labels[44:], [e.get('origin') for e in audit[:3]])
    with open(st.p('audit-final-only.jsonl'), 'w') as f:            # what a single read at the end can say
        s, rid = await st.rt('GET', '/status')
        f.write(json.dumps({'t': time.time(), 'runtime_id': rid.get('runtime_id'), 'runtime_id_after': rid.get('runtime_id'), 'audit': audit}) + '\n')
    rc, rep, err = await off_loop(reconcile, st, st.p('audit-final-only.jsonl'), 'final-only')
    notes['final_only'] = rep and rep['counts']
    ck('from one final read: the 44 lost entries are "uncheckable_turnover", never "not written" and never consistent',
       rep is not None and rep['counts'].get('uncheckable_turnover') == 44 and rep['counts'].get('consistent') == 256, (rc, err[-300:], notes['final_only']))
    ck('...and the reconciliation is CLEAN', rc == 0, rc)
    lost = {r['label'] for r in (rep or {}).get('actions', []) if r['class'] == 'uncheckable_turnover'}
    ck('...and the uncheckable ones are exactly the 44 oldest', lost == set(labels[:44]), len(lost))
    rc, rep, err = await off_loop(reconcile, st, st.p('audit-archive.jsonl'), 'archive')
    notes['archive'] = rep and rep['counts']
    ck('from the once-a-second archive: all 300 consistent with journal and device, CLEAN',
       rc == 0 and rep is not None and rep['counts'].get('consistent') == 300 and not rep['audit_archive']['turnover_gaps'], (rc, err[-300:], notes['archive']))


# ---- B ---------------------------------------------------------------------------------------------------------------------
async def case_B5(st, ck, notes):
    await asyncio.sleep(1.0)
    code, line, err = await off_loop(other_bridge, st, registry_text(st.targets), 'b5-preflight', ['--preflight', 'on'])
    notes['with_preflight'] = {'exit': code, 'stderr': err.strip()[-300:]}
    ck('(a) with the start-up preflight on, the bridge refuses this registry (exit 4) and names the variable',
       code == 4 and 'no_such_var' in err, notes['with_preflight'])
    # the running bridge was started with --preflight off: what the request path does when the name is wrong
    p, ev = await submit_wait(st, 'bogus', 1.0)
    ck('(a) a registry entry naming no variable: rejected / unknown_variable', short(ev) == ('rejected', 'unknown_variable'), short(ev))
    ck('(a) the bridge disabled the entry', any(r['kind'] == 'disable' and r.get('target_key') == 'bogus' for r in st.journal()))
    resp = [r for r in st.wire_all() if r.get('ev') == 'resp' and r.get('status') == 404]
    ck('(a) IA2 answered 404', len(resp) == 1, len(resp))
    n_posts = len(posts(st))
    p2, ev2 = await submit_wait(st, 'bogus', 2.0)
    ck('(a) a second request to the disabled entry is refused before IA2', ev2 is not None and ev2['status'] == 'rejected' and len(posts(st)) == n_posts,
       (short(ev2), len(posts(st))))
    notes['disabled_reason'] = short(ev2)
    # (b) a throw-away instance told to stop
    s, body = await st.rt('POST', '/stop', {}, timeout=3.0)
    notes['stop_answer'] = [s, body]
    await asyncio.sleep(0.5)
    n_posts = len(posts(st))
    p3, ev3 = await submit_wait(st, 'flow_setpoint', 5.0, timeout=20)
    notes['after_stop'] = short(ev3)
    ck('(b) after POST /stop the write is rejected, not accepted', ev3 is not None and ev3['status'] == 'rejected', short(ev3))
    ck('(b) ...before it reached IA2 (the gates see the stopped runtime first)', len(posts(st)) == n_posts, wire_summary(st.wire()))
    notes['note'] = ('(b) EXPECT was a 409 "scan loop has stopped" mapped to scan_stopped. The bridge\'s gates reject first (G8: the runtime '
                     'is gone or its scan count stands still), so the 409 path is unreachable from the bridge; its mapping is a unit vector '
                     '(tests/test_pure.py).')


async def case_B10(st, ck, notes):
    await asyncio.sleep(1.0)
    p1, ev1 = await submit_wait(st, 'flow_setpoint', 5.0, simulation=False)
    ck('registry simulation=true, request false: rejected / simulation_mismatch', short(ev1) == ('rejected', 'simulation_mismatch'), short(ev1))
    p2, ev2 = await submit_wait(st, 'flow_live', 5.0, simulation=True)
    ck('registry simulation=false, request true: rejected / simulation_mismatch', short(ev2) == ('rejected', 'simulation_mismatch'), short(ev2))
    await asyncio.sleep(1.0)
    ck('nothing reached IA2', not posts(st), wire_summary(st.wire()))
    ck('IA2 /audit is empty', await audit_list(st) == [])


# ---- C ---------------------------------------------------------------------------------------------------------------------
async def case_C2(st, ck, notes):
    await asyncio.sleep(1.0)
    p = st.standin.submit('flow_setpoint', 12.5)
    ck('the bridge passed the gates and paused before the send', await wait_marker(st, 'after_gates'))
    s, body = await st.rt('POST', '/inject-scan-stall', {'stall_ms': 100, 'scans': 10}, timeout=3.0)
    tripped = None
    for _ in range(100):
        sh, h = await st.rt('GET', '/health', timeout=1.0)
        if isinstance(h, dict) and h.get('watchdog_tripped') is True:
            tripped = True
            break
        await asyncio.sleep(0.1)
    ck('the watchdog tripped while the bridge waited', tripped is True)
    release(st, 'after_gates')
    ev = await wait_until(lambda: st.event(p['action_id']), 15)
    ck('the write is unknown / latched_after_write, never accepted', short(ev) == ('unknown', 'latched_after_write'), short(ev))
    conns = {q.get('conn') for q in posts(st)}
    notes['ia2_write_status'] = [r.get('status') for r in st.wire_all() if r.get('ev') == 'resp' and r.get('conn') in conns]
    ck('IA2 answered the POST 200 (it takes writes while latched)', notes['ia2_write_status'] == [200], notes['ia2_write_status'])
    ck('exactly one POST', len(posts(st)) == 1)


async def case_C5(st, ck, notes):
    await asyncio.sleep(1.0)
    # (a) paused
    s, body = await st.rt('POST', '/pause', {}, timeout=3.0)
    notes['pause'] = [s, body]
    p, ev = await submit_wait(st, 'flow_setpoint', 6.0)
    notes['while_paused'] = short(ev)
    ck('(a) paused: rejected, nothing sent', ev is not None and ev['status'] == 'rejected' and not posts(st), (short(ev), len(posts(st))))
    ck('(a) ...with reason paused or (scan count frozen) runtime_unreachable', short(ev) in (('rejected', 'paused'), ('rejected', 'runtime_unreachable')), short(ev))
    sd, d = await rt_direct('sp_flow', f32bits(6.5), 'lab-c5-direct')
    await asyncio.sleep(0.8)
    sc, status = await st.rt('GET', '/status')
    notes['direct_write_while_paused'] = {'answer': [sd, d], 'vm_sp_flow': vars_of(status).get('sp_flow'),
                                          'device_got_6_5': bool([e for e in st.slave_writes(100) if e.get('value') == 0x40D0])}
    await st.rt('POST', '/resume', {}, timeout=3.0)
    await asyncio.sleep(1.0)
    # (b) target forced
    s, body = await st.rt('POST', '/force', {'name': 'sp_flow', 'value': f32bits(3.0)}, timeout=3.0)
    notes['force'] = [s, body]
    await asyncio.sleep(0.5)
    n_posts = len(posts(st))
    p2, ev2 = await submit_wait(st, 'flow_setpoint', 7.0)
    ck('(b) forced: rejected / forced, nothing sent', short(ev2) == ('rejected', 'forced') and len(posts(st)) == n_posts, (short(ev2), len(posts(st))))
    sd, d = await rt_direct('sp_flow', f32bits(7.5), 'lab-c5-direct')
    await asyncio.sleep(0.8)
    sc, status = await st.rt('GET', '/status')
    notes['direct_write_while_forced'] = {'answer': [sd, d], 'vm_sp_flow': vars_of(status).get('sp_flow'),
                                          'device_got_7_5': bool([e for e in st.slave_writes(100) if e.get('value') == 0x40F0])}
    await st.rt('POST', '/unforce', {'name': 'sp_flow'}, timeout=3.0)
    notes['note'] = ('The bridge has no switch to skip G9, so the "diagnostic" half is IA2 itself: a direct POST /write while paused and '
                     'while forced, recorded as characterization.')


async def case_C7(st, ck, notes):
    await asyncio.sleep(1.0)
    await st.slave.set_mode('close')
    t = await healthy(st, 'coupler', False, timeout=20)
    ck('the runtime flagged coupler unhealthy', t is not None)
    sh, h = await st.rt('GET', '/health')
    notes['health_status_while_unhealthy'] = h.get('status') if isinstance(h, dict) else h
    ck('/health.status is still ok', isinstance(h, dict) and h.get('status') == 'ok', notes['health_status_while_unhealthy'])
    p, ev = await submit_wait(st, 'c7_flow', 5.0)
    ck('rejected / device_unhealthy', short(ev) == ('rejected', 'device_unhealthy'), short(ev))
    await asyncio.sleep(1.0)
    ck('nothing reached IA2', not posts(st), wire_summary(st.wire()))
    ck('IA2 /audit is empty', await audit_list(st) == [])
    await st.slave.set_mode('normal')
    await healthy(st, 'coupler', True, timeout=30)


async def case_C10(st, ck, notes):
    await asyncio.sleep(1.0)
    nb = st.p('project/northbound.toml')
    reg = registry_text(st.targets)
    results = {}
    for tag, text in (('a_allow_true', '[mqtt]\nallow_write = true\n'), ('b_unparseable', '[mqtt\nallow_write = \n'),
                      ('b_unreadable', '[mqtt]\nallow_write = false\n'), ('d_allow_false', '[mqtt]\nallow_write = false\n')):
        with open(nb, 'w') as f:
            f.write(text)
        if tag == 'b_unreadable':
            os.chmod(nb, 0)
        try:
            code, line, err = await off_loop(other_bridge, st, reg, tag)
        finally:
            os.chmod(nb, stat.S_IRUSR | stat.S_IWUSR)
        results[tag] = {'exit': code, 'stderr': err.strip()[-200:]}
    os.remove(nb)
    code, line, err = await off_loop(other_bridge, st, reg, 'c_absent')
    results['c_absent'] = {'exit': code, 'stderr': err.strip()[-200:]}
    notes['startups'] = results
    for tag in ('a_allow_true', 'b_unparseable', 'b_unreadable'):
        ck('(%s) the bridge refuses to start with exit code 3' % tag, results[tag]['exit'] == 3, results[tag])
    for tag in ('c_absent', 'd_allow_false'):
        ck('(%s) the bridge starts' % tag, results[tag]['exit'] is None, results[tag])
    # (e) the file changes under the running bridge
    p0, ev0 = await submit_wait(st, 'flow_setpoint', 5.0)
    ck('(e) before the change a write is accepted', short(ev0) == ('accepted', None), short(ev0))
    await asyncio.sleep(5.5)                                  # out of the single-flight window
    with open(nb, 'w') as f:
        f.write('[mqtt]\nenabled = false\nallow_write = true\n')
    n_posts = len(posts(st))
    p1, ev1 = await submit_wait(st, 'flow_setpoint', 6.0)
    ck('(e) after the change: rejected / northbound_write_enabled', short(ev1) == ('rejected', 'northbound_write_enabled'), short(ev1))
    ck('(e) ...without reaching IA2', len(posts(st)) == n_posts, wire_summary(st.wire()))
    os.remove(nb)
    notes['note'] = ('(e) The bridge re-reads the file before every request, so it refuses at once; the runtime restart in the EXPECT is not '
                     'needed for the refusal and was not done (the runtime would only adopt the file at its next start).')


# ---- R ---------------------------------------------------------------------------------------------------------------------
async def case_R4(st, ck, notes):
    await asyncio.sleep(1.0)
    hold = st.p('hold')
    st.bridge_args += ['--hold-file', hold]
    await st.stop_bridge()
    await st.start_bridge()
    p0, ev0 = await submit_wait(st, 'flow_setpoint', 5.0)
    ck('before the hold: accepted', short(ev0) == ('accepted', None), short(ev0))
    with open(hold, 'w') as f:
        f.write('operator hold\n')
    await asyncio.sleep(5.5)
    p1, ev1 = await submit_wait(st, 'flow_setpoint', 6.0)
    ck('hold present: rejected / held', short(ev1) == ('rejected', 'held'), short(ev1))
    await st.stop_bridge()
    await st.start_bridge()
    p2, ev2 = await submit_wait(st, 'flow_setpoint', 7.0)
    ck('after a bridge restart the hold still holds', short(ev2) == ('rejected', 'held'), short(ev2))
    ck('the earlier write is untouched (still accepted, its value still on the device)',
       short(st.event(p0['action_id'])) == ('accepted', None) and st.slave.regs.get(100) == 0x40A0, (short(st.event(p0['action_id'])), st.slave.regs.get(100)))
    os.remove(hold)
    p3, ev3 = await submit_wait(st, 'flow_setpoint', 8.0)
    ck('hold removed: accepted again', short(ev3) == ('accepted', None), short(ev3))
    ck('two POSTs in all (no retraction, nothing for the held ones)', len(posts(st)) == 2, wire_summary(st.wire()))


R5_KEYS = ['r5_%02d' % i for i in range(41)]
R5_TARGETS = [tgt(k, 'd2_sp', 'REAL', 0.0, 40.0, require_devices=[], require_fieldbus_healthy=False, single_flight_ms=0, max_deadline_ms=120000)
              for k in R5_KEYS] + [tgt('r5_one', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], max_deadline_ms=120000,
                                       verify=fb('sp_flow_fb'))]


async def case_R5(st, ck, notes):
    """55 writes earlier in the minute, then the burst.  A burst alone cannot reach the bridge-wide limit: the bridge reads one line at a
    time (about 7 writes a second here), and a line it reaches more than 2 s after `requested_at_ms` is `expired` (G4).  The first run
    of this case sent 50 at once after 30 and got 12 `expired`, 0 `rate_limited`."""
    await asyncio.sleep(1.0)
    pre = []
    for i in range(55):                                        # in batches of five, each answered before the next
        pre.append(st.standin.submit(R5_KEYS[i % len(R5_KEYS)], 1.0 + i * 0.25, ttl_ms=120000))
        if i % 5 == 4:
            await wait_until(lambda: all(st.event(p['action_id']) for p in pre), 20)
    ck('55 earlier writes accepted', all(short(st.event(p['action_id'])) == ('accepted', None) for p in pre),
       [short(st.event(p['action_id'])) for p in pre if short(st.event(p['action_id'])) != ('accepted', None)][:5])
    t0 = time.time()
    burst = []
    for i in range(50):
        key = 'r5_one' if i < 10 else R5_KEYS[i - 10 + 1]
        burst.append(st.standin.submit(key, 2.0 + i * 0.1, ttl_ms=120000))
        await asyncio.sleep(max(0.0, t0 + (i + 1) * 0.019 - time.time()))
    notes['burst_seconds'] = round(time.time() - t0, 3)
    ck('50 requests inside one second', notes['burst_seconds'] < 1.0, notes['burst_seconds'])
    await wait_until(lambda: all(st.event(p['action_id']) for p in burst), 110)
    out = [short(st.event(p['action_id'])) for p in burst]
    one = out[:10]
    ck('same target: exactly one let through, the other 9 busy', one.count(('accepted', None)) == 1 and one.count(('rejected', 'busy')) == 9, one)
    notes['outcomes'] = {str(o): out.count(o) for o in set(out)}
    ck('the other 40: four more fill the bridge-wide 60 (55 + 1 + 4), the other 36 rate_limited',
       out[10:].count(('accepted', None)) == 4 and out[10:].count(('rejected', 'rate_limited')) == 36, notes['outcomes'])
    times = [r['t_ms'] for r in st.journal() if r['kind'] == 'intent']
    busiest = max(len([x for x in times if q - 60000 < x <= q]) for q in times) if times else 0
    notes['busiest_60s_intents'] = busiest
    ck('never more than 60 intents in any 60 s, and the limit was reached', busiest == 60, busiest)
    await asyncio.sleep(1.0)
    audit = await audit_list(st)
    sent = len([r for r in st.journal() if r['kind'] == 'intent'])
    ck('IA2 /audit entries = writes the bridge let through', len(audit) == sent == len(posts(st)), (len(audit), sent, len(posts(st))))


# ---- F ---------------------------------------------------------------------------------------------------------------------
async def case_F3(st, ck, notes):
    await asyncio.sleep(0.5)
    with open(st.p('registry.json')) as f:
        text = f.read()
    changed = text.replace('"max": 40.0', '"max": 40.5', 1)
    ck('one value in the copy differs', changed != text)
    reg = st.p('registry-f3.json')
    with open(reg, 'w') as f:
        f.write(changed)
    r = await off_loop(lambda: subprocess.run(st.bridge_cmd(registry=reg, registry_sha=st.registry_sha, journal=st.p('journal-f3.jsonl')),
                                              capture_output=True, text=True, timeout=20, env=dict(os.environ, PYTHONDONTWRITEBYTECODE='1')))
    notes['refusal'] = r.stderr.strip()[-300:]
    ck('the bridge refuses to start (exit 2) on the pinned hash', r.returncode == 2, (r.returncode, notes['refusal']))
    ck('...and says the registry hash does not match', 'sha256' in r.stderr.lower() or 'hash' in r.stderr.lower(), notes['refusal'])


async def case_F4(st, ck, notes):
    await asyncio.sleep(0.5)
    wide = tgt('wide', 'sp_flow', 'REAL', 0.0, 50.0)                  # governance allows 0..40
    upper = tgt('upper', 'SP_FLOW', 'REAL', 0.0, 40.0)                # not the runtime's spelling
    inst = tgt('inst', 'main_inst.sp_flow', 'REAL', 0.0, 40.0)        # instance-qualified
    sc, status = await st.rt('GET', '/status')
    names = set(vars_of(status))
    findings = check_registry.check([wide, upper, inst, FLOW], st.p('project'), snapshot_names=names)
    notes['findings'] = findings
    notes['without_snapshot'] = check_registry.check([inst], st.p('project'))
    text = json.dumps(findings)
    ck('the range outside governance is flagged for `wide`', 'wide' in text and 'range_outside_governance' in text, findings)
    ck('`SP_FLOW` is flagged', 'upper' in text, findings)
    ck('`main_inst.sp_flow` is flagged', 'inst' in text, findings)
    ck('the correct entry is not flagged', 'flow_setpoint' not in text, findings)
    ck('the runtime snapshot spells it sp_flow, and neither SP_FLOW nor main_inst.sp_flow', 'sp_flow' in names and not {'SP_FLOW', 'main_inst.sp_flow'} & names,
       sorted(names))


async def case_F5(st, ck, notes):
    await asyncio.sleep(0.5)
    gated = tgt('gated', 'big', 'DINT', -1000, 1000, require_devices=['coupler'], requires_approval=True)
    code, line, err = await off_loop(other_bridge, st, registry_text([FLOW, gated]), 'f5-enabled')
    notes['enabled'] = {'exit': code, 'stderr': err.strip()[-300:]}
    ck('enabled with requires_approval: the bridge refuses to start (exit 2)', code == 2, notes['enabled'])
    ck('...and names the entry', 'gated' in err, err[-300:])
    off = dict(gated, enabled=False)
    code, line, err = await off_loop(other_bridge, st, registry_text([FLOW, off]), 'f5-disabled')
    notes['disabled'] = {'exit': code, 'stderr': err.strip()[-300:]}
    ck('disabled: the bridge starts', code is None, notes['disabled'])
    # the running bridge's registry has the disabled entry: its requests never reach IA2
    p, ev = await submit_wait(st, 'gated', 5)
    ck('a request to the disabled entry is rejected before IA2', ev is not None and ev['status'] == 'rejected' and not posts(st), (short(ev), len(posts(st))))
    ck('IA2 /audit is empty', await audit_list(st) == [])


C7T = tgt('c7_flow', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], require_fieldbus_healthy=False)
LIVE = tgt('flow_live', 'sp_flow', 'REAL', 0.0, 40.0, require_devices=['coupler'], simulation=False)
BOGUS = tgt('bogus', 'no_such_var', 'REAL', 0.0, 10.0, require_devices=['coupler'])
GATED_OFF = tgt('gated', 'big', 'DINT', -1000, 1000, require_devices=['coupler'], requires_approval=True, enabled=False)
SYNC = {'hooks': HOOKS_SYNC, 'env': {'HOOK_AT': 'after_gates', 'HOOK_DIR': '{dir}'}}
WARN = {'bridge_args': ['--governance-check', 'warn']}

CASES = {
    'A3': (case_A3, [FLOW]),
    'A4': (case_A4, [NOFB, BIG_NOFB]),
    'B5': (case_B5, [FLOW, BOGUS], {'bridge_args': ['--governance-check', 'warn', '--preflight', 'off']}),
    'B10': (case_B10, [FLOW, LIVE]),
    'C2': (case_C2, [FLOW], SYNC),
    'C5': (case_C5, [FLOW]),
    'C7': (case_C7, [C7T]),
    'C10': (case_C10, [FLOW]),
    'R4': (case_R4, [FLOW]),
    'R5': (case_R5, R5_TARGETS, WARN),
    'F3': (case_F3, [FLOW]),
    'F4': (case_F4, [FLOW]),
    'F5': (case_F5, [FLOW, GATED_OFF]),
}
