#!/usr/bin/env python3
"""COMMON.md gate for both independent opt-ins and their combination.

Uses private caches/logs and fails on printed mismatches (older self-tests may
return status zero even when mismatches occur). No GUI is started.
"""
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/phase2-c/checks'
OUT.mkdir(parents=True, exist_ok=True)
BIN = ROOT / 'target/release/attesor'
base = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
base['TMPDIR'] = '/private/tmp'
classes = 'blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks'.split()
for label, flags in [('off', ''), ('slotreuse', 'slotreuse'), ('jitcost', 'jitcost'), ('both', 'jitcost,slotreuse')]:
    directory = OUT / label
    directory.mkdir(exist_ok=True)
    env = dict(base, ATTESOR_JIT_ON=flags, ATTESOR_CACHE=str(directory / 'cache'),
               ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0')
    def run(name, command, timeout=600):
        r = subprocess.run(command, cwd=ROOT, env=env, text=True, capture_output=True, timeout=timeout)
        (directory / (name + '.log')).write_text(r.stdout + r.stderr)
        assert r.returncode == 0, (label, name, r.stdout[-2000:], r.stderr[-2000:])
        return r.stdout
    text = run('cargo-test', ['cargo', 'test', '--release', '--workspace', '--', '--test-threads=1'])
    print(label, '; '.join(re.findall(r'^test result:.*$', text, re.M)), flush=True)
    for cls in classes + ['jitcode']:
        seeds = ['1', '0x1234', '0xdeadbeef'] if cls in ['loops', 'jitcode'] else [None]
        for seed in seeds:
            count = '512' if cls == 'jitcode' else '2000'
            cmd = [str(BIN), '--selftest-jit', cls, count] + ([seed] if seed else [])
            text = run(cls + ('-' + seed if seed else ''), cmd)
            mismatches = re.findall(r'mismatch=(\d+)', text)
            assert mismatches and all(int(n) == 0 for n in mismatches), (label, cls, text)
            print(label, text.strip().splitlines()[-1], flush=True)
    text = run('loopra-aot', ['bash', 'tests/attesor/loopra-aot-check.sh', str(BIN)])
    assert 'PASS loopra AOT: all 7 workloads' in text, text
    print(label, text.strip().splitlines()[-1], flush=True)
print('PASS COMMON gate: workspace, all 13 classes, three loop/relocation seeds, private cold/warm AOT, all four flag combinations')
