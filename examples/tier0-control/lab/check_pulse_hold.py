"""How long a pulse write holds the fake device's coil when the write waited behind a slow scan (REPAIR-ADVICE FIX03).

  python3 -I -B lab/check_pulse_hold.py --bin ../../target/release/ia2-runtime --out evidence/pulse-hold [--runs 3]
          [--reference NAME=BIN ...]

For each pulse length (800 ms, longer than the wait; 100 ms, shorter) it injects one 300 ms scan stall, writes `lamp` with
`pulse_ms` 60 ms later, and measures the hold on the device: from the coil-on write to the coil-off write in the fake device's
own log, by its monotonic `mono` stamps.  `/status` sampling is too coarse for this.  The binary under test must hold every pulse
within the band below; --reference binaries are measured and reported, not judged.

The band is fixed in advance from the lab's timing: the program task runs every 10 ms and pushes changed outputs every scan, and
the Modbus task sends a queued write at once.  The pulse clock starts when the write has landed in the VM; the coil-on write
reaches the device within one scan after that, so the hold can be at most one task interval short.  The reset is applied at the
next command drain and sent in that scan: allow five task intervals late.  Loopback only: no claim about real-time behaviour."""
import argparse
import asyncio
import hashlib
import json
import os
import sys
import time

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import harness  # noqa: E402

TASK_MS = 10
EARLY_MS, LATE_MS = TASK_MS, 5 * TASK_MS
STALL_MS, WRITE_AFTER_MS = 300, 60
PULSES = (800, 100)


def hold_ms(log_path):
    """(on, off, held ms) from the first answered FC05 coil-0 write of 1 and the next of 0."""
    on = off = None
    with open(log_path) as f:
        for line in f:
            r = json.loads(line)
            if r.get('ev') != 'req' or r.get('action') != 'resp' or r.get('fc') != 5 or r.get('addr') != 0:
                continue
            if on is None and r.get('value') == 0xFF00:
                on = r
            elif on is not None and r.get('value') == 0:
                off = r
                break
    if on is None or off is None:
        return on, off, None
    return on, off, round((off['mono'] - on['mono']) * 1000, 3)


async def one(out, binary, pulse_ms):
    st = harness.Stack(out, binary, [harness.FLOW])
    try:
        await st.start()
        await asyncio.sleep(0.5)
        sc, _ = await st.rt('POST', '/inject-scan-stall', {'stall_ms': STALL_MS, 'scans': 1})
        await asyncio.sleep(WRITE_AFTER_MS / 1000.0)
        t0 = time.monotonic()
        sc_w, body = await st.rt('POST', '/write', {'name': 'lamp', 'value': 1, 'pulse_ms': pulse_ms})
        answered_ms = round((time.monotonic() - t0) * 1000, 1)
        await asyncio.sleep((pulse_ms + 600) / 1000.0)
    finally:
        await st.stop()
    on, off, held = hold_ms(os.path.join(out, 'slave-1.jsonl'))
    return {'pulse_ms': pulse_ms, 'stall_status': sc, 'write_status': sc_w, 'write_body': body, 'write_answered_after_ms': answered_ms,
            'on_seq': on and on.get('seq'), 'off_seq': off and off.get('seq'), 'held_ms': held}


async def main(args):
    os.makedirs(args.out, exist_ok=True)
    bins = [('under_test', args.bin)] + [tuple(r.split('=', 1)) for r in args.reference]
    rows = []
    for name, binary in bins:
        digest = hashlib.sha256(open(binary, 'rb').read()).hexdigest()
        for pulse_ms in PULSES:
            for n in range(1, (args.runs if name == 'under_test' else 1) + 1):
                row = dict(await one(os.path.join(args.out, '%s-%d-r%d' % (name, pulse_ms, n)), binary, pulse_ms),
                           binary=name, binary_sha256=digest, run=n)
                if name == 'under_test':
                    row['ok'] = (row['write_status'] == 200 and row['held_ms'] is not None
                                 and pulse_ms - EARLY_MS <= row['held_ms'] <= pulse_ms + LATE_MS)
                rows.append(row)
                print(json.dumps({k: row.get(k) for k in ('binary', 'pulse_ms', 'run', 'write_status', 'held_ms', 'ok')}), flush=True)
    judged = [r for r in rows if r['binary'] == 'under_test']
    summary = {'band_ms': {'early': EARLY_MS, 'late': LATE_MS}, 'stall_ms': STALL_MS, 'write_after_ms': WRITE_AFTER_MS,
               'verdict': 'PASS' if judged and all(r['ok'] for r in judged) else 'FAIL', 'runs': rows}
    with open(os.path.join(args.out, 'pulse-hold.json'), 'w') as f:
        json.dump(summary, f, indent=1)
    print(summary['verdict'])
    return 0 if summary['verdict'] == 'PASS' else 1


if __name__ == '__main__':
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--bin', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--runs', type=int, default=3)
    ap.add_argument('--reference', action='append', default=[], metavar='NAME=BIN')
    sys.exit(asyncio.run(main(ap.parse_args())))
