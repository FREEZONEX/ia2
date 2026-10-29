#!/usr/bin/env python3
"""Stage 1a soak (DESIGN-1A WP9, TEST-CASES section 4 exit criterion 3): random valid, invalid, duplicate and forged requests
for a fixed time against the real bridge, a sandbox ia2-runtime and two fake Modbus devices, with faults injected throughout; then
the independent three-way reconciliation (tools/reconcile_report.py) must find no unknown write and no unexplained difference.
Loopback only; the only processes it signals are its own children.  Python >= 3.8, stdlib.

  python3 -I lab/soak.py --bin <ia2-runtime> --out evidence/soak-<date> [--minutes 120] [--seed N]

Faults (one at a time, every 2-4 minutes): a device link closed / refused / silent, the runtime killed and restarted, the runtime
stopped (SIGSTOP) for a few seconds, a watchdog latch then a restart, the bridge killed at a random moment, and three exact points
taken from the P0 cases through an armed hook (lab/hooks_soak.py): killed after the intent (R1), killed after the response (R2),
and the runtime stopped while the write is queued behind a scan stall (R3T1).  Once, around the middle, the /audit archiver
pauses long enough for the 256-entry ring to turn over.

The directory gets everything the cases keep (journal, trace, wire, device and runtime logs) plus audit-archive.jsonl.gz (one
line per /audit poll, about once a second, read straight from the runtime as the read-only collector does), lifecycle.jsonl
(runtime instances, faults, bridge starts), device-map.json, reconcile.json and soak-summary.json.  Exit 0 on PASS."""
import argparse
import asyncio
import gzip
import json
import os
import platform
import random
import resource
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import harness  # noqa: E402
import mkproject_ctl  # noqa: E402
from cases_p0 import BIG, D2, FLOW, LAMP, tgt  # noqa: E402
from harness import ROOT, now_ms, sha256_file, verify_chain  # noqa: E402

HOOKS_SOAK = os.path.join(ROOT, 'lab', 'hooks_soak.py')
RECONCILE = os.path.join(ROOT, 'tools', 'reconcile_report.py')
DENIED = tgt('unlisted', 'unlisted_sp', 'REAL', 0.0, 10.0, require_devices=['coupler'])     # not in the project's governance: 403
TARGETS = [FLOW, BIG, LAMP, D2, DENIED]
MEM_LIMIT_KB = 3 * 1024 * 1024        # any one of our processes above 3 GB RSS stops the soak (a leak, not a result)

REQUEST_MIX = [('valid', 50), ('denied', 5), ('out_of_range', 5), ('unknown_key', 3), ('forged', 4), ('malformed', 3),
               ('expired', 3), ('simulation', 2), ('duplicate', 10), ('burst', 8), ('bad_type', 2)]
FAULT_MIX = [('device2_outage', 3), ('device1_silent', 2), ('runtime_restart', 2), ('runtime_stop', 2), ('latch', 1),
             ('bridge_kill', 2), ('r1_kill_after_intent', 1), ('r2_kill_after_response', 1), ('r3t1_stop_while_queued', 2)]


PROGRAM_ASSIGNS = {'prog_sp': 1069547520}      # mkproject_ctl's main.st: prog_sp := 1.5 on every scan (REAL bits)


def device_map():
    """IA2 output variable -> the device role, encoding and address it is written to (from the lab project's own tables)."""
    chans = {(dev, c[0]): c for dev, chs in mkproject_ctl.DEVICES.items() for c in chs}
    role = {'coupler': '1', 'second': '2'}
    out = {}
    for var, direction, dev, chan in mkproject_ctl.MAPS:
        if direction != 'output':
            continue
        _, _, addr, kind, dtype = chans[(dev, chan)]
        out[var] = {'device': role[dev], 'kind': 'coil' if kind == 'coil' else dtype, 'addr': addr}
        if var in PROGRAM_ASSIGNS:
            out[var]['program_value'] = PROGRAM_ASSIGNS[var]
    return out


def pick(rng, mix):
    return rng.choices([k for k, _ in mix], [w for _, w in mix])[0]


class Soak:
    def __init__(self, args):
        self.args, self.rng = args, random.Random(args.seed)
        self.dir = args.out
        self.st = harness.Stack(self.dir, args.bin, TARGETS, bridge_args=['--governance-check', 'warn'])
        self.env = {'HOOK_DIR': self.dir}
        self.counts = {k: 0 for k, _ in REQUEST_MIX}
        self.faults = []
        self.submitted = []               # every authentic payload: its action_id must get an answer
        self.lines = []                   # authentic envelope lines, for duplicates
        self.last_value = {}
        self.unexpected = []              # a bridge or runtime that exited when no fault asked it to
        self.expect_bridge_down = self.expect_runtime_down = False
        self.archiver_paused = False
        self.no_restart_until = 0.0
        self.stop_flag = False
        self.mem_tripped = None

    # ---- records ---------------------------------------------------------------------------------------------------------
    def life(self, ev, **fields):
        rec = dict(fields, ev=ev, t=round(time.time(), 6))
        self.lifefile.write(json.dumps(rec, sort_keys=True) + '\n')
        self.lifefile.flush()
        return rec

    async def runtime_id(self):
        s, body = await self.st.rt('GET', '/status', timeout=2.0)
        return body.get('runtime_id') if s == 200 and isinstance(body, dict) else None

    async def note_runtime_ready(self, spawned_t):
        self.life('runtime_ready', spawned_t=round(spawned_t, 6), runtime_id=await self.runtime_id(), n=self.st.runtime_n)

    # ---- requests ----------------------------------------------------------------------------------------------------------
    def value_for(self, key):
        last = self.last_value.get(key)
        while True:
            if key == 'lamp':
                v = not last if last is not None else True
            elif key == 'big_setpoint':
                v = self.rng.choice([x for x in range(-1000, 1001, 7) if x != 0])
            elif key == 'unlisted':
                v = round(self.rng.uniform(0.5, 9.5), 3)
            else:
                v = round(self.rng.uniform(0.5, 39.5), 3)
            if v != last:
                self.last_value[key] = v
                return v

    def submit(self, key, value, **kw):
        sa = self.st.standin
        p = sa.payload(key, value, **kw)
        line = sa.envelope(sa.payload_text(p))
        sa.append_line(line)
        self.submitted.append(p['action_id'])
        self.lines.append(line)
        return p

    def request(self, kind):
        sa = self.st.standin
        self.counts[kind] += 1
        valid_keys = ['flow_setpoint', 'big_setpoint', 'lamp', 'd2_set']
        if kind == 'valid':
            key = self.rng.choice(valid_keys)
            self.submit(key, self.value_for(key))
        elif kind == 'denied':
            self.submit('unlisted', self.value_for('unlisted'))
        elif kind == 'out_of_range':
            self.submit(*self.rng.choice([('flow_setpoint', 55.5), ('big_setpoint', 5000), ('d2_set', -3.0)]))
        elif kind == 'unknown_key':
            self.submit(self.rng.choice(['no_such_key', 'sp_flow', 'main_inst.sp_flow']), 1.0)
        elif kind == 'forged':
            p = sa.payload(self.rng.choice(valid_keys), 7.0)
            sa.append_line(sa.envelope(sa.payload_text(p), secret=os.urandom(32)))
        elif kind == 'malformed':
            sa.append_line(self.rng.choice(['not json %d' % self.counts[kind], '{"v":"ia2.control/v0","n":%d' % self.counts[kind],
                                            '[%d]' % self.counts[kind]]))
        elif kind == 'expired':
            t = now_ms()
            self.submit('flow_setpoint', 3.0, requested_at_ms=t - 30000, deadline_ms=t - 10000)
        elif kind == 'simulation':
            self.submit('flow_setpoint', 4.0, simulation=False)
        elif kind == 'bad_type':
            self.submit(self.rng.choice(['flow_setpoint', 'lamp']), self.rng.choice(['12.5', None, [1]]))
        elif kind == 'duplicate' and self.lines:
            sa.append_line(self.rng.choice(self.lines[-50:]))
        elif kind == 'burst':
            key = self.rng.choice(valid_keys)
            self.submit(key, self.value_for(key))
            self.submit(key, self.value_for(key))

    async def request_loop(self, end):
        while not self.stop_flag and time.time() < end:
            self.request(pick(self.rng, REQUEST_MIX))
            await asyncio.sleep(self.rng.uniform(1.0, 2.2))

    # ---- the /audit archive: what the read-only collector does, once a second, straight to the runtime ------------------------
    async def archiver(self):
        prev = None
        with gzip.open(self.st.p('audit-archive.jsonl.gz'), 'at') as f:
            while not self.stop_flag:
                t = time.time()
                if not self.archiver_paused:
                    rid = await self.runtime_id()
                    s, audit = await self.st.rt('GET', '/audit', timeout=2.0)
                    rid2 = await self.runtime_id()
                    rec = {'t': round(time.time(), 6), 'runtime_id': rid, 'runtime_id_after': rid2}
                    if s == 200 and isinstance(audit, list):
                        if prev == (rid, audit) and rid == rid2:
                            rec['same'] = True
                        else:
                            rec['audit'] = audit
                            prev = (rid, audit)
                    else:
                        rec['error'] = str(audit)[:200]
                    f.write(json.dumps(rec, sort_keys=True, separators=(',', ':')) + '\n')
                    f.flush()
                await asyncio.sleep(max(0.05, 1.0 - (time.time() - t)))

    # ---- faults ------------------------------------------------------------------------------------------------------------
    async def restart_runtime(self):
        self.expect_runtime_down = True
        t = time.time()
        await self.st.restart_runtime()
        await self.note_runtime_ready(t)
        self.expect_runtime_down = False

    async def restart_bridge(self, kill):
        self.expect_bridge_down = True
        if kill:
            await self.st.kill_bridge()
        else:
            await self.st.stop_bridge()
        self.life('bridge_down', how='SIGKILL' if kill else 'SIGTERM', returncode=self.st.bridge.returncode)
        await asyncio.sleep(self.rng.uniform(0.5, 3.0))
        info = await self.st.start_bridge(HOOKS_SOAK, self.env)
        self.life('bridge_up', n=self.st.bridge_n, pid=info.get('pid') if isinstance(info, dict) else None)
        self.expect_bridge_down = False

    def hook_file(self, kind, point):
        return self.st.p('%s-%s' % (kind, point))

    def clear_hook(self, point):
        for kind in ('arm', 'reached', 'release'):
            try:
                os.remove(self.hook_file(kind, point))
            except FileNotFoundError:
                pass

    async def armed(self, point, timeout=30):
        """Arm one hook point and wait until a request reaches it.  The request generator supplies the write."""
        self.clear_hook(point)
        open(self.hook_file('arm', point), 'w').close()
        end = time.time() + timeout
        while time.time() < end:
            if os.path.exists(self.hook_file('reached', point)):
                return True
            await asyncio.sleep(0.01)
        self.clear_hook(point)
        return False

    def release(self, point):
        with open(self.hook_file('release', point), 'w') as f:
            f.write('go')

    async def fault(self, kind):
        st, rng = self.st, self.rng
        rec = self.life('fault_start', kind=kind)
        detail = {}
        if kind == 'device2_outage':
            mode, secs = rng.choice(['close', 'refuse', 'silent']), rng.uniform(6, 15)
            await st.slave2.set_mode(mode)
            await asyncio.sleep(secs)
            await st.slave2.set_mode('normal')
            detail = {'mode': mode, 'seconds': round(secs, 1)}
        elif kind == 'device1_silent':
            secs = rng.uniform(3, 6)
            await st.slave.set_mode('silent')
            await asyncio.sleep(secs)
            await st.slave.set_mode('normal')
            detail = {'seconds': round(secs, 1)}
        elif kind == 'runtime_restart':
            await self.restart_runtime()
        elif kind == 'runtime_stop':
            secs = rng.uniform(2, 5)
            st.signal_runtime(signal.SIGSTOP)
            await asyncio.sleep(secs)
            st.signal_runtime(signal.SIGCONT)
            detail = {'seconds': round(secs, 1)}
        elif kind == 'latch':
            self.life('safe_state', why='watchdog latch injected')        # the latch drives every output to its failsafe value
            s, body = await st.rt('POST', '/inject-scan-stall', {'stall_ms': 100, 'scans': 10}, timeout=3.0)
            latched = None
            for _ in range(100):
                s2, h = await st.rt('GET', '/health', timeout=1.0)
                if isinstance(h, dict) and h.get('watchdog_tripped') is True:
                    latched = True
                    break
                await asyncio.sleep(0.1)
            await asyncio.sleep(rng.uniform(8, 15))          # requests meanwhile meet G8: rejected / latched
            await self.restart_runtime()
            detail = {'inject': s, 'latched': latched}
        elif kind == 'bridge_kill':
            await asyncio.sleep(rng.uniform(0, 1.5))
            await self.restart_bridge(kill=True)
        elif kind in ('r1_kill_after_intent', 'r2_kill_after_response'):
            point = 'after_intent' if kind.startswith('r1') else 'after_response'
            detail['reached'] = await self.armed(point)
            if detail['reached']:
                await self.restart_bridge(kill=True)
            self.clear_hook(point)
        elif kind == 'r3t1_stop_while_queued':
            detail['reached'] = await self.armed('after_intent')
            if detail['reached']:
                s, body = await st.rt('POST', '/inject-scan-stall', {'stall_ms': 1000, 'scans': 1}, timeout=3.0)
                self.release('after_intent')
                await asyncio.sleep(0.15)
                st.signal_runtime(signal.SIGSTOP)
                await asyncio.sleep(3.5)                       # past the bridge's 3 s POST timeout
                st.signal_runtime(signal.SIGCONT)
                detail['stall'] = s
            await asyncio.sleep(0.5)
            self.clear_hook('after_intent')
        self.faults.append(dict(kind=kind, t=rec['t'], **detail))
        self.life('fault_end', kind=kind, **detail)

    async def fault_loop(self, end, archiver_gap_at, archiver_gap_s):
        gap_done = False
        while not self.stop_flag and time.time() < end - 60:
            await asyncio.sleep(self.rng.uniform(*self.args.fault_every))
            if self.stop_flag or time.time() >= end - 60:
                break
            if not gap_done and archiver_gap_s > 0 and time.time() >= archiver_gap_at:
                gap_done = True
                self.archiver_paused = True
                self.no_restart_until = time.time() + archiver_gap_s + 5
                self.life('archiver_pause', seconds=archiver_gap_s)
                asyncio.get_running_loop().call_later(archiver_gap_s, self.resume_archiver)
            kind = pick(self.rng, FAULT_MIX)
            while time.time() < self.no_restart_until and kind in ('runtime_restart', 'latch'):
                kind = pick(self.rng, FAULT_MIX)     # the ring must turn over inside ONE runtime instance
            await self.fault(kind)

    def resume_archiver(self):
        self.archiver_paused = False
        self.life('archiver_resume')

    # ---- watch -------------------------------------------------------------------------------------------------------------
    def rss_kb(self, pid):
        r = subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True, text=True)
        return int(r.stdout.strip() or 0)

    async def monitor(self, t_start, end):
        while not self.stop_flag:
            await asyncio.sleep(15)
            st = self.st
            if not self.expect_bridge_down and st.bridge.returncode is not None:
                self.unexpected.append(self.life('bridge_exited_unexpectedly', returncode=st.bridge.returncode, n=st.bridge_n))
                await self.restart_bridge(kill=False)
            if not self.expect_runtime_down and st.proc.poll() is not None:
                self.unexpected.append(self.life('runtime_exited_unexpectedly', returncode=st.proc.returncode, n=st.runtime_n))
                await self.restart_runtime()
            mem = {'soak': resource.getrusage(resource.RUSAGE_SELF).ru_maxrss // (1024 if sys.platform == 'darwin' else 1),
                   'bridge': self.rss_kb(st.bridge.pid), 'runtime': self.rss_kb(st.proc.pid)}
            if max(mem.values()) > MEM_LIMIT_KB:
                self.mem_tripped = mem
                self.life('memory_limit', **mem)
                self.stop_flag = True
            for events in st.slave_events.values():          # the files keep them; memory does not need to
                events.clear()
            print('[%5.1f min] requests=%d faults=%d unexpected=%d rss_kb=%s' % (
                (time.time() - t_start) / 60, sum(self.counts.values()), len(self.faults), len(self.unexpected), mem), flush=True)

    # ---- run ---------------------------------------------------------------------------------------------------------------
    async def run(self):
        args, st = self.args, self.st
        t_start = time.time()
        await st.start(hooks=HOOKS_SOAK, env=self.env)
        st.sampler.interval_s = 1.0                       # one sample a second is enough for the read side here
        self.lifefile = open(st.p('lifecycle.jsonl'), 'a')
        await self.note_runtime_ready(t_start)
        self.life('bridge_up', n=st.bridge_n)
        with open(st.p('device-map.json'), 'w') as f:
            json.dump(device_map(), f, indent=1, sort_keys=True)
        end = t_start + args.minutes * 60
        archiver = asyncio.ensure_future(self.archiver())
        monitor = asyncio.ensure_future(self.monitor(t_start, end))
        gap_s = args.archiver_gap_minutes * 60
        error = None
        try:
            await asyncio.gather(self.request_loop(end),
                                 self.fault_loop(end, t_start + args.minutes * 60 * 0.45, gap_s))
            self.life('requests_end')
            await asyncio.sleep(20)                             # second reconcile looks, deliveries, and archive coverage
            await st.snapshot('final')
            self.life('safe_state', why='the harness stops the runtime (POST /stop runs the failsafe pass)')
        except Exception:  # noqa: BLE001
            import traceback
            error = traceback.format_exc()
        finally:
            self.stop_flag = True
            for task in (archiver, monitor):
                task.cancel()
            try:
                await st.stop()
            except Exception as exc:  # noqa: BLE001
                error = (error or '') + '\nstop failed: %r' % exc
            self.lifefile.close()
        return self.conclude(t_start, error)

    def conclude(self, t_start, error):
        st = self.st
        rep_path = st.p('reconcile.json')
        cmd = [sys.executable, '-I', '-B', RECONCILE, '--journal', st.p('journal.jsonl'), '--audit-archive', st.p('audit-archive.jsonl.gz'),
               '--device-map', st.p('device-map.json'), '--device-log', '1=' + st.p('slave-1.jsonl'), '--device-log', '2=' + st.p('slave-2.jsonl'),
               '--lifecycle', st.p('lifecycle.jsonl'), '--wire', st.p('wire.jsonl'), '--out', rep_path]
        rc = subprocess.run(cmd, capture_output=True, text=True)
        report = json.load(open(rep_path)) if os.path.exists(rep_path) else None
        trace = st.trace()
        answered = {e.get('action_id') for e in trace if e.get('kind') == 'action_status'}
        statuses = {}
        for e in trace:
            if e.get('kind') == 'action_status':
                k = '%s/%s' % (e.get('status'), e.get('reason'))
                statuses[k] = statuses.get(k, 0) + 1
        kinds = {}
        for r in st.journal():
            kinds[r['kind']] = kinds.get(r['kind'], 0) + 1
        chain_ok, n_records = verify_chain(st.p('journal.jsonl'))
        unanswered = [a for a in self.submitted if a not in answered]
        checks = {
            'reconciliation is CLEAN (no unknown write, no unexplained difference)': rc.returncode == 0,
            'the journal hash chain verifies': chain_ok,
            'no bridge or runtime exited unless a fault made it': not self.unexpected,
            'every authentic request got an answer': not unanswered,
            'the bridge never wrote `verified`': all(e.get('status') != 'verified' for e in trace),
            'the soak ran its full length': self.mem_tripped is None and error is None,
        }
        summary = {
            'verdict': 'PASS' if all(checks.values()) else 'FAIL', 'checks': checks, 'error': error,
            'minutes_planned': self.args.minutes, 'minutes_run': round((time.time() - t_start) / 60, 1), 'seed': self.args.seed,
            'requests': self.counts, 'authentic_submitted': len(self.submitted), 'unanswered': unanswered[:20],
            'faults': self.faults, 'fault_counts': {k: sum(1 for f in self.faults if f['kind'] == k) for k, _ in FAULT_MIX},
            'unexpected_exits': self.unexpected, 'memory_tripped': self.mem_tripped,
            'trace_statuses': dict(sorted(statuses.items())), 'journal_kinds': dict(sorted(kinds.items())), 'journal_records': n_records,
            'runtime_instances': st.runtime_n, 'bridge_starts': st.bridge_n,
            'reconcile': None if report is None else {'verdict': report['verdict'], 'counts': report['counts'],
                                                      'turnover_gaps': report['audit_archive']['turnover_gaps'],
                                                      'instances': len(report['audit_archive']['instances'])},
            'reconcile_exit': rc.returncode, 'reconcile_stderr': rc.stderr[-2000:],
            'runtime_bin': self.args.bin, 'runtime_bin_sha256': sha256_file(self.args.bin),
            'code_tree_sha256': harness.tree_hash(harness.code_files() + ['tools/reconcile_report.py']),
            'python': sys.version.split()[0], 'platform': platform.platform(), 'registry_sha256': getattr(st, 'registry_sha', None),
        }
        with open(st.p('soak-summary.json'), 'w') as f:
            json.dump(summary, f, indent=1, sort_keys=True)
        lines = subprocess.run(['/bin/sh', '-c', 'cd "%s" && find . -type f ! -name MANIFEST.sha256 -print0 | sort -z | xargs -0 shasum -a 256' % st.dir],
                               capture_output=True, text=True).stdout
        with open(st.p('MANIFEST.sha256'), 'w') as f:
            f.write(lines)
        return summary


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--bin', required=True)
    ap.add_argument('--out', required=True, help='a new directory')
    ap.add_argument('--minutes', type=float, default=120)
    ap.add_argument('--archiver-gap-minutes', type=float, default=12, help='the one /audit archiver pause (0 for none)')
    ap.add_argument('--fault-every', type=float, nargs=2, default=(120.0, 240.0), metavar=('MIN_S', 'MAX_S'))
    ap.add_argument('--seed', type=int, default=None)
    args = ap.parse_args()
    args.out, args.bin = os.path.abspath(args.out), os.path.abspath(args.bin)
    if args.seed is None:
        args.seed = int.from_bytes(os.urandom(4), 'big')
    if os.path.exists(args.out):
        raise SystemExit('%s exists; use a new directory' % args.out)
    summary = asyncio.run(Soak(args).run())
    print(json.dumps({k: summary[k] for k in ('verdict', 'checks', 'minutes_run', 'requests', 'fault_counts', 'reconcile')}, indent=1))
    raise SystemExit(0 if summary['verdict'] == 'PASS' else 1)


if __name__ == '__main__':
    main()
