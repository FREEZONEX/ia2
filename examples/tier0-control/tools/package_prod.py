#!/usr/bin/env python3
"""Builds a PRODUCTION copy of the bridge in <dest>: flavor 'prod', the lab-only module left out, every block between
`# --- lab-only begin` and `# --- lab-only end` removed, and then checks that nothing about hooks is left in it.

  python3 -I tools/package_prod.py <dest>        (run from the 1a directory; <dest> is replaced)"""
import ast
import os
import re
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LAB_ONLY_FILES = {'testhooks_loader.py'}
FORBIDDEN = ('test-hooks', 'test_hooks', 'testhooks', 'importlib')
BLOCK = re.compile(r'^[ \t]*# --- lab-only begin\n.*?^[ \t]*# --- lab-only end\n', re.S | re.M)


def build(dest):
    shutil.rmtree(dest, ignore_errors=True)
    os.makedirs(os.path.join(dest, 'bridge'))
    for name in ('bridge_run.py', 'contract_vocab.json'):
        shutil.copy(os.path.join(ROOT, name), os.path.join(dest, name))
    # Preserve the pinned parser, provenance and license in the standalone copy.
    shutil.copytree(os.path.join(ROOT, 'bridge', '_vendor'), os.path.join(dest, 'bridge', '_vendor'),
                    ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
    for name in sorted(os.listdir(os.path.join(ROOT, 'bridge'))):
        if not name.endswith('.py') or name in LAB_ONLY_FILES:
            continue
        with open(os.path.join(ROOT, 'bridge', name), encoding='utf-8') as f:
            text = f.read()
        if name == 'build_flavor.py':
            text = '"""Production build."""\nFLAVOR = \'prod\'\n'
        text = BLOCK.sub('', text)
        with open(os.path.join(dest, 'bridge', name), 'w', encoding='utf-8') as f:
            f.write(text)
    problems = []
    for base, _, files in os.walk(dest):
        for name in files:
            if name.endswith('.py'):
                path = os.path.join(base, name)
                with open(path, encoding='utf-8') as f:
                    text = f.read()
                ast.parse(text, path)
                problems += ['%s mentions %r' % (os.path.relpath(path, dest), w) for w in FORBIDDEN if w in text]
    if os.path.exists(os.path.join(dest, 'bridge', 'testhooks_loader.py')):
        problems.append('the lab-only loader is in the package')
    if "FLAVOR = 'prod'" not in open(os.path.join(dest, 'bridge', 'build_flavor.py')).read():
        problems.append('flavor is not prod')
    return problems


if __name__ == '__main__':
    bad = build(sys.argv[1])
    for p in bad:
        print('PACKAGE PROBLEM:', p, file=sys.stderr)
    sys.exit(1 if bad else 0)
