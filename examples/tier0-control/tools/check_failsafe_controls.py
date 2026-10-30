"""Require nullable-failsafe regressions to detect three deliberately broken variants."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'evidence' / 'failsafe-controls'
OUT.mkdir(parents=True, exist_ok=True)
ACTIVE = "return any(any(doc['failsafe_status'].values()) for doc in docs if doc.get('failsafe_status') is not None)"
CASES = [
    ('reject-null', "if doc.get('failsafe_status') is not None:", "if 'failsafe_status' in doc:",
     'test_startup_accepts_absent_null_and_empty_reports'),
    ('ignore-other-report', ACTIVE, 'return False',
     'test_null_does_not_mask_pending_acknowledged_or_unconfirmed'),
    ('accept-falsy-malformed', "if doc.get('failsafe_status') is not None:", "if doc.get('failsafe_status'):",
     'test_malformed_report_is_rejected_even_when_peer_is_null'),
]
results = []
for name, old, new, method in CASES:
    with tempfile.TemporaryDirectory(prefix='ia2-failsafe-control-') as tmp:
        dest = Path(tmp)
        for directory in ('bridge', 'tests', 'readside'):
            shutil.copytree(ROOT / directory, dest / directory, ignore=shutil.ignore_patterns('__pycache__'))
        shutil.copy2(ROOT / 'contract_vocab.json', dest / 'contract_vocab.json')
        path = dest / 'bridge/runtime_schema.py'
        text = path.read_text()
        assert text.count(old) == 1, name
        path.write_text(text.replace(old, new))
        p = subprocess.run([sys.executable, '-I', '-B', str(dest / 'tests/test_failsafe_compat.py'),
                            'NullableFailsafe.' + method], capture_output=True, text=True, timeout=60)
        log = p.stdout + p.stderr
        detected = p.returncode != 0 and 'AssertionError' in log and 'FAILED (failures=' in log and 'ERROR:' not in log
        (OUT / (name + '.log')).write_text(log)
        row = {'mutation': name, 'detected_by_assertion': detected, 'exit_code': p.returncode}
        results.append(row)
        print(json.dumps(row), flush=True)
(OUT / 'summary.json').write_text(json.dumps(results, indent=2) + '\n')
sys.exit(0 if all(r['detected_by_assertion'] for r in results) else 1)
