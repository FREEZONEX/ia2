"""Prove D11 tests reject five deliberately broken authentication variants."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
CASES = [
    ('skip_mac', 'auth.py', 'if not hmac.compare_digest(expected, env[\'sig\']):',
     'if False and not hmac.compare_digest(expected, env[\'sig\']):'),
    ('wrong_role_keys', 'verifier.py', "verified_records(samples, sample_keys, 'sample', revoked)",
     "verified_records(samples, event_keys, 'sample', revoked)"),
    ('ignore_hint_mismatch', 'auth.py', "if 'hint' in env and env['hint'] != _hint(record, role):",
     "if False and 'hint' in env and env['hint'] != _hint(record, role):"),
    ('ignore_revocation', 'auth.py', "if key is None or env['key_id'] in revoked:", 'if key is None:'),
    ('verify_at_current_time', 'auth.py', "if not key['not_before_ms'] <= _stamp(record, role) <= key['not_after_ms']:",
     "if not key['not_before_ms'] <= __import__('time').time() * 1000 <= key['not_after_ms']:"),
]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--out', type=Path, required=True)
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    results = []
    for name, module, old, new in CASES:
        with tempfile.TemporaryDirectory(prefix='ia2-d11-mutation-') as tmp:
            dest = Path(tmp)
            for directory in ('bridge', 'readside', 'tests'):
                shutil.copytree(ROOT / directory, dest / directory, ignore=shutil.ignore_patterns('__pycache__'))
            shutil.copy2(ROOT / 'contract_vocab.json', dest / 'contract_vocab.json')
            path = dest / 'readside' / module
            text = path.read_text()
            assert text.count(old) == 1, name
            path.write_text(text.replace(old, new))
            run = subprocess.run([sys.executable, '-I', '-B', str(dest / 'tests/test_d11_auth.py')],
                                 capture_output=True, text=True, timeout=60)
            output = run.stdout + run.stderr
            detected = run.returncode != 0 and 'AssertionError' in output and 'FAILED (failures=' in output and 'ERROR:' not in output
            (args.out / (name + '.log')).write_text(output)
            result = dict(mutation=name, detected_by_assertion=detected, exit_code=run.returncode)
            results.append(result)
            print(json.dumps(result), flush=True)
    (args.out / 'summary.json').write_text(json.dumps(results, indent=2) + '\n')
    return 0 if all(r['detected_by_assertion'] for r in results) else 1


if __name__ == '__main__':
    sys.exit(main())
