"""Assert each crash-recovery action separately, using only saved local lab evidence."""
import json
from pathlib import Path
import sys
SRC=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(SRC))
from readside.auth import load_keys, verify
RUNS=Path(sys.argv[1]) if len(sys.argv)>1 else SRC.parent/'runs-final'

def rows(path):return [json.loads(l) for l in path.read_text().splitlines() if l.strip()]

out=[]
for case,expected in [('R1',0),('R2',1)]:
    root=RUNS/(case+'-r1');journal=rows(root/'journal.jsonl');trace=[verify(e,load_keys(root/'event-keys.json'),'event') for e in rows(root/'trace.jsonl')]
    wire=rows(root/'wire.jsonl');audit=json.loads((root/'final.json').read_text())['audit']['body']
    intents=[r for r in journal if r['kind']=='intent']
    assert len(intents)==(1 if case=='R1' else 2)
    for intent in intents:
        aid,label=intent['action_id'],intent['label']
        posts=[r for r in wire if r.get('ev')=='req' and r.get('method')=='POST' and r.get('origin')==label]
        matches=[r for r in audit if r.get('origin')==label]
        terminal=[r for r in journal if r['kind']=='terminal' and r.get('action_id')==aid]
        events=[r for r in trace if r.get('kind')=='action_status' and r.get('action_id')==aid]
        assert len(posts)==len(matches)==expected,(case,aid,posts,matches)
        assert len(terminal)==len(events)==1,(case,aid)
        assert terminal[0]['event']['status']==('unknown' if expected==0 else 'accepted')
        if expected:
            body=json.loads(posts[0]['body'])
            assert body=={'name':intent['variable'],'value':intent['bits']}
            assert matches[0]['name']==intent['variable']
            assert matches[0]['requested']==matches[0]['applied']==intent['bits']
        out.append(dict(case=case,action_id=aid,label=label,posts=len(posts),audit_rows=len(matches),
                        terminal_records=len(terminal),trace_events=len(events),status=terminal[0]['event']['status']))
path=SRC/'evidence'/'crash-per-action.json'
path.write_text(json.dumps(out,indent=2)+'\n')
print('PASS',len(out),'crash-boundary actions checked individually;',path)
