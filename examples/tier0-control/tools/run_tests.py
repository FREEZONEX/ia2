"""Run isolated stdlib test suites serially; stop with a failure if any suite fails."""
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'evidence' / 'unit'
OUT.mkdir(parents=True, exist_ok=True)
results = []
for path in sorted((ROOT / 'tests').glob('test_*.py')):
    with (OUT / (path.stem + '.log')).open('w') as log:
        try:
            result = subprocess.run([sys.executable, '-I', '-B', str(path)], cwd=ROOT,
                                    stdout=log, stderr=subprocess.STDOUT, timeout=180)
            code = result.returncode
        except subprocess.TimeoutExpired:
            code = 124
    text = (OUT / (path.stem + '.log')).read_text()
    count = re.search(r'Ran (\d+) tests?', text)
    row = {'suite': path.stem, 'exit_code': code, 'tests': int(count.group(1)) if count else 0}
    results.append(row)
    print(json.dumps(row), flush=True)
summary = {'suites': results, 'tests': sum(r['tests'] for r in results),
           'passed': all(r['exit_code'] == 0 and r['tests'] > 0 for r in results)}
(OUT / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
sys.exit(0 if summary['passed'] else 1)
