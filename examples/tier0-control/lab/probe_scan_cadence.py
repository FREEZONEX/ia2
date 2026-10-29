#!/usr/bin/env python3
"""Probe: how often does the runtime's PUBLISHED scan_count advance?  Reads /status every 20 ms for 3 s against a
sandbox runtime (10 ms task) and prints each change.  Usage: python3 -I lab/probe_scan_cadence.py <ia2-runtime> <scratch-dir>"""
import asyncio
import os
import shutil
import sys
import time

sys.dont_write_bytecode = True
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
for p in (ROOT, os.path.join(ROOT, 'lab'), os.path.join(ROOT, 'vendor-copies')):
    sys.path.insert(0, p)
import harness  # noqa: E402


async def main(binpath, scratch):
    st = harness.Stack(scratch, binpath, [harness.FLOW])
    try:
        await st.start()
        await asyncio.sleep(1.0)
        t0, last, last_t, steps = time.time(), None, None, []
        while time.time() - t0 < 3.0:
            s, b = await st.rt('GET', '/status')
            if s == 200:
                c, t = b['scan_count'], time.time()
                if last is not None and c != last:
                    steps.append((round((t - last_t) * 1000), c - last))
                if c != last:
                    last, last_t = c, t
            await asyncio.sleep(0.02)
        print('scan_period_ms in /status = 10; /status read every ~20 ms for 3 s')
        print('each change of the published scan_count: (ms since the previous change, scans in the step)')
        print(steps)
        gaps = [g for g, _ in steps]
        print('steps=%d  gap min/median/max = %d/%d/%d ms  scans per step min/max = %d/%d' % (
            len(gaps), min(gaps), sorted(gaps)[len(gaps) // 2], max(gaps), min(n for _, n in steps), max(n for _, n in steps)))
    finally:
        await st.stop()
        shutil.rmtree(scratch, ignore_errors=True)

asyncio.run(main(sys.argv[1], sys.argv[2]))
