"""F01 local acceptance: per-action POST, audit, snapshot and fake coil assertions."""
import argparse
import asyncio
import json
import math
from pathlib import Path
import sys
sys.dont_write_bytecode=True
SRC=Path(__file__).resolve().parents[1]
sys.path[:0]=[str(SRC),str(SRC/'lab')]
import harness
from bridge.engine import make_label

async def main(binary):
    root=SRC/'evidence'/'bool-device'
    n=1
    while root.exists():
        n+=1;root=SRC/'evidence'/('bool-device-%d'%n)
    root.mkdir(parents=True)
    results=[]
    try:
        for lo,hi in [(0,0),(1,1),(0,1)]:
            entry=harness.target('lamp_limit','lamp','BOOL',lo,hi,require_devices=['coupler'],
                                 min_interval_ms=0,single_flight_ms=0,max_rate_per_min=60,
                                 verify=dict(feedback='lamp_fb',tolerance=0,settle_ms=5000))
            st=harness.Stack(str(root/('%d-%d'%(lo,hi))),binary,[entry])
            try:
                await st.start();await asyncio.sleep(.4)
                for value in (False,True,0,1,0.0,1.0,-0.0):
                    before=st.slave.coils.get(0,0)
                    p=st.standin.submit('lamp_limit',value,ttl_ms=10000)
                    label=make_label(p)
                    ev=await harness.wait_until(lambda:st.event(p['action_id']),5)
                    assert ev, 'no action terminal'
                    await asyncio.sleep(.3)
                    _,audit=await st.rt('GET','/audit')
                    _,status=await st.rt('GET','/status')
                    posts=[r for r in st.wire() if r['method']=='POST' and r.get('origin')==label]
                    matches=[r for r in audit if r.get('origin')==label]
                    bits={v['name']:v['bits'] for v in status['last_snapshot']['vars'] if v['name'] in ('lamp','lamp_fb')}
                    negzero=type(value)is float and value==0 and math.copysign(1,value)<0
                    allowed=not negzero and lo<=int(value)<=hi
                    expected=int(value) if allowed else before
                    result=dict(bounds=[lo,hi],input=repr(value),action_id=p['action_id'],label=label,
                                status=ev['status'],reason=ev.get('reason'),posts=len(posts),audit_rows=len(matches),
                                before=before,after=st.slave.coils.get(0,0),snapshot=bits,allowed=allowed)
                    results.append(result)
                    assert ev['status']==('accepted' if allowed else 'rejected'),result
                    assert len(posts)==len(matches)==int(allowed),result
                    assert bits=={'lamp':expected,'lamp_fb':expected},result
                    assert st.slave.coils.get(0,0)==expected,result
                    if not allowed:assert ev['reason']==('bad_value' if negzero else 'out_of_range'),result
                print('PASS BOOL range',lo,hi,flush=True)
            finally: await st.stop()
    finally:
        (root/'results.json').write_text(json.dumps(results,indent=2)+'\n')
    print('PASS',len(results),'per-action fake-device checks;',root,flush=True)

if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--bin', required=True, help='path to an ia2-runtime binary')
    asyncio.run(main(str(Path(parser.parse_args().bin).resolve())))
