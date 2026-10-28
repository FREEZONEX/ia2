"""Startup check that the registry stays inside the project's own write governance (W11; CONTRACT 3.2 "应当").

The runtime exposes its governance over no HTTP route, but the deployed project directory is on the bridge's host, so the
`[governance]` table of project.toml can be read (line by line: Python 3.9 has no TOML parser).  Findings:
  not_in_governance          the variable has no rule although write_mode = "allowlist"
  range_outside_governance   the registry range is not inside the rule's min/max
Modes: strict (refuse to start), warn (record in the journal), off."""
import os
import re


def parse(project_toml_text):
    mode, rules, cur = 'open', {}, None
    for line in project_toml_text.splitlines():
        line = line.split('#', 1)[0].strip()
        m = re.match(r'write_mode\s*=\s*"(\w+)"', line)
        if m:
            mode = m.group(1)
        if line.startswith('[[governance.rules]]'):
            cur = {}
            continue
        if line.startswith('['):
            cur = None
        if cur is not None:
            m = re.match(r'(variable|min|max)\s*=\s*(.+)', line)
            if m:
                value = m.group(2).strip()
                try:
                    cur[m.group(1)] = value.strip('"') if m.group(1) == 'variable' else float(value)
                except ValueError:
                    cur[m.group(1)] = float('nan')        # unreadable bound: treated as a violation below
                if 'variable' in cur:
                    rules[cur['variable'].lower()] = cur
    return mode, rules


def check(targets, project_dir):
    """`targets`: iterable of registry entries.  Returns a list of {'key','kind','detail'}; raises OSError if project.toml is unreadable."""
    with open(os.path.join(project_dir, 'project.toml'), encoding='utf-8') as f:
        mode, rules = parse(f.read())
    findings = []
    for t in targets:
        if not t['enabled']:
            continue
        name = t['ia2_variable']
        rule = rules.get(name.lower()) or rules.get(name.split('.')[-1].lower())
        if rule is None:
            if mode == 'allowlist':
                findings.append({'key': t['key'], 'kind': 'not_in_governance', 'detail': '%s has no governance rule' % name})
            continue
        lo, hi = rule.get('min', float('-inf')), rule.get('max', float('inf'))
        if not (lo <= t['min'] and t['max'] <= hi):           # also false for NaN bounds
            findings.append({'key': t['key'], 'kind': 'range_outside_governance',
                             'detail': 'registry %s..%s is not inside governance %s..%s' % (t['min'], t['max'], rule.get('min'), rule.get('max'))})
    return findings
