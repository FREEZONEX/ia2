#!/usr/bin/env python3
"""Lab project for the control-write cases (stdlib, Python >= 3.8).  New file; the WP0 generator is left as evidence.

Device "coupler" (Modbus TCP, loopback) and device "second" (a second slave).  Every target has a write channel and,
where the case needs a read-back, a read-only "echo" channel at the SAME address, which the fake slave serves from the
same register / coil table:

  coupler  sp_flow     REAL  holding 100 f32   write / echo sp_echo  -> variable sp_flow_fb     governance 0..40
           big         DINT  holding 120 i32   write / echo big_echo -> big_fb                   governance -1000..1000
           lamp        BOOL  coil 0            write / echo lamp_in  -> lamp_fb                  governance (no bounds)
           prog_sp     REAL  holding 130 f32   write / echo prog_echo-> prog_fb                  governance 0..10
                                               and the PROGRAM assigns prog_sp := 1.5 on every scan
           unlisted_sp REAL  holding 140 f32   write only                                        NOT in the governance allowlist
  second   d2_sp       REAL  holding 100 f32   write / echo d2_echo   -> d2_fb                   governance 0..40

No northbound.toml unless `northbound_text` is given."""
import hashlib
import os
import shutil

REAL_CH = ('holding_register', 'f32')
DEVICES = {
    'coupler': [
        ('sp_out', 'write', 100, 'holding_register', 'f32'), ('sp_echo', 'read', 100, 'holding_register', 'f32'),
        ('big_out', 'write', 120, 'holding_register', 'i32'), ('big_echo', 'read', 120, 'holding_register', 'i32'),
        ('lamp_out', 'write', 0, 'coil', None), ('lamp_in', 'read', 0, 'coil', None),
        ('prog_out', 'write', 130, 'holding_register', 'f32'), ('prog_echo', 'read', 130, 'holding_register', 'f32'),
        ('unl_out', 'write', 140, 'holding_register', 'f32')],
    'second': [('d2_out', 'write', 100, 'holding_register', 'f32'), ('d2_echo', 'read', 100, 'holding_register', 'f32')],
}
MAPS = [('sp_flow', 'output', 'coupler', 'sp_out'), ('sp_flow_fb', 'input', 'coupler', 'sp_echo'),
        ('big', 'output', 'coupler', 'big_out'), ('big_fb', 'input', 'coupler', 'big_echo'),
        ('lamp', 'output', 'coupler', 'lamp_out'), ('lamp_fb', 'input', 'coupler', 'lamp_in'),
        ('prog_sp', 'output', 'coupler', 'prog_out'), ('prog_fb', 'input', 'coupler', 'prog_echo'),
        ('unlisted_sp', 'output', 'coupler', 'unl_out'),
        ('d2_sp', 'output', 'second', 'd2_out'), ('d2_fb', 'input', 'second', 'd2_echo')]
VARS = [('sp_flow', 'REAL'), ('sp_flow_fb', 'REAL'), ('big', 'DINT'), ('big_fb', 'DINT'), ('lamp', 'BOOL'), ('lamp_fb', 'BOOL'),
        ('prog_sp', 'REAL'), ('prog_fb', 'REAL'), ('unlisted_sp', 'REAL'), ('d2_sp', 'REAL'), ('d2_fb', 'REAL')]
GOVERNANCE = [('sp_flow', 0.0, 40.0), ('big', -1000.0, 1000.0), ('lamp', None, None), ('prog_sp', 0.0, 10.0), ('d2_sp', 0.0, 40.0)]


def _device_toml(name, port, channels):
    lines = ['name = "%s"' % name, 'protocol = "modbus"', 'slave_id = 1', 'poll_interval_ms = 100', 'timeout_ms = 1000', '',
             '[transport]', 'kind = "tcp"', 'host = "127.0.0.1"', 'port = %d' % port, '']
    for cname, access, addr, kind, dtype in channels:
        lines += ['[[channels]]', 'name = "%s"' % cname, 'kind = "%s"' % kind, 'access = "%s"' % access, 'address = %d' % addr]
        if dtype:
            lines += ['data_type = "%s"' % dtype, 'word_order = "hi_lo"']
        lines += ['']
    return '\n'.join(lines)


def render(dest, port, port2=None, task_ms=10, northbound_text=None, big_range=None):
    """`big_range` = (min, max) replaces the governance range of `big` (default -1000..1000); the P1 cases that write full-width DINT values
    need a project whose governance lets them through, or the bridge's own check would refuse the write before the verifier is exercised."""
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(os.path.join(dest, 'devices'))
    os.makedirs(os.path.join(dest, 'pous'))
    ports = {'coupler': port, 'second': port2 if port2 is not None else port + 1}
    for name, channels in DEVICES.items():
        with open(os.path.join(dest, 'devices', name + '.toml'), 'w') as f:
            f.write(_device_toml(name, ports[name], channels))
    with open(os.path.join(dest, 'iomap.toml'), 'w') as f:
        f.write('\n'.join('[[mappings]]\napplication = "main"\nvariable = "%s"\ndirection = "%s"\ndevice = "%s"\nchannel = "%s"\n' % m
                          for m in MAPS))
    gov = ['name = "control_lab"', 'version = "0.2"', '', '[governance]', 'write_mode = "allowlist"', '']
    for var, lo, hi in GOVERNANCE:
        if var == 'big' and big_range is not None:
            lo, hi = big_range
        gov += ['[[governance.rules]]', 'variable = "%s"' % var]
        if lo is not None:
            gov += ['min = %s' % lo, 'max = %s' % hi]
        gov += ['']
    with open(os.path.join(dest, 'project.toml'), 'w') as f:
        f.write('\n'.join(gov))
    with open(os.path.join(dest, 'tasks.toml'), 'w') as f:
        f.write('[[tasks]]\nname = "motion"\ninterval_ms = %d\npriority = 1\n\n'
                '[[programs]]\ninstance = "main_inst"\nprogram = "main"\ntask = "motion"\n' % task_ms)
    decls = ['    ticks : UDINT := 0;'] + ['    %s : %s;' % v for v in VARS]
    with open(os.path.join(dest, 'pous', 'main.st'), 'w') as f:
        f.write('PROGRAM main\nVAR\n' + '\n'.join(decls) + '\nEND_VAR\n\n'
                '(* prog_sp is assigned on EVERY scan: a monitor write to it is overwritten (case C6). *)\n'
                'ticks := (ticks + 1) MOD 1000000000;\nprog_sp := 1.5;\nEND_PROGRAM\n')
    if northbound_text is not None:
        with open(os.path.join(dest, 'northbound.toml'), 'w') as f:
            f.write(northbound_text)
    return tree_hashes(dest)


def tree_hashes(root):
    out = {}
    for base, _, files in sorted(os.walk(root)):
        for name in sorted(files):
            path = os.path.join(base, name)
            with open(path, 'rb') as f:
                out[os.path.relpath(path, root)] = hashlib.sha256(f.read()).hexdigest()
    return out
