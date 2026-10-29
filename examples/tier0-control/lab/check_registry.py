"""Offline registry check (DESIGN-1A F-8, partial): reads a registry and the deployed project directory and reports
  assigned_by_program        the target's IA2 variable is assigned in the ST program (a monitor write would be overwritten)
  not_declared               the variable is not declared in any VAR block (the name would 404)
  not_in_governance          the variable has no governance rule although the project runs in allowlist mode
  range_outside_governance   the registry range is not inside the governance rule's range
  not_in_snapshot            (only when the running runtime's /status variable names are given) the name is not spelled exactly as
                             the runtime spells it -- case, an instance prefix -- so a write to it would 404
It is a conservative TEXT scan, not a parser: comments are stripped, `name :=` is looked for.  Approximate by design; the
runtime result of case C6 is the evidence, this is the early warning."""
import os
import re


def _strip_st_comments(text):
    text = re.sub(r'\(\*.*?\*\)', lambda m: re.sub(r'[^\n]', ' ', m.group(0)), text, flags=re.S)
    return re.sub(r'//[^\n]*', '', text)


def governance(project_toml):
    from bridge import governance_check
    return governance_check.parse(project_toml)


def check(targets, project_dir, snapshot_names=None):
    findings = []
    with open(os.path.join(project_dir, 'project.toml'), encoding='utf-8') as f:
        mode, rules = governance(f.read())
    st_texts = []
    pous = os.path.join(project_dir, 'pous')
    for name in sorted(os.listdir(pous)):
        if name.endswith('.st'):
            with open(os.path.join(pous, name), encoding='utf-8') as f:
                st_texts.append((name, _strip_st_comments(f.read())))
    for t in targets:
        if not t.get('enabled', True):
            continue
        short = t['ia2_variable'].split('.')[-1]
        if snapshot_names is not None and t['ia2_variable'] not in snapshot_names:
            findings.append({'key': t['key'], 'kind': 'not_in_snapshot',
                             'detail': '%s is not a variable name in the running runtime\'s /status snapshot' % t['ia2_variable']})
        if not any(re.search(r'(?m)^\s*%s\s*:[^=]' % re.escape(short), txt) for _, txt in st_texts):
            findings.append({'key': t['key'], 'kind': 'not_declared', 'detail': '%s is not declared in any VAR block' % short})
        for name, txt in st_texts:
            for n, line in enumerate(txt.splitlines(), 1):
                if re.match(r'\s*(?:\w+\.)?%s\s*:=' % re.escape(short), line):
                    findings.append({'key': t['key'], 'kind': 'assigned_by_program', 'detail': '%s:%d assigns %s' % (name, n, short)})
        rule = rules.get(short.lower()) or rules.get(t['ia2_variable'].lower())
        if mode == 'allowlist' and rule is None:
            findings.append({'key': t['key'], 'kind': 'not_in_governance', 'detail': '%s has no governance rule' % short})
        elif rule is not None and ('min' in rule or 'max' in rule):
            if t['min'] < rule.get('min', float('-inf')) or t['max'] > rule.get('max', float('inf')):
                findings.append({'key': t['key'], 'kind': 'range_outside_governance',
                                 'detail': 'registry %s..%s vs governance %s..%s' % (t['min'], t['max'], rule.get('min'), rule.get('max'))})
    return findings
