#!/usr/bin/env python3
"""Deliberately break each new generated path; restore sources/build in finally.

Run only in this private worktree, with no concurrent builds. The two mutants
must print differential MISMATCH lines; a crash/timeout alone is not a proof.
"""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/phase2-c/sensitivity'
OUT.mkdir(parents=True, exist_ok=True)
BIN = ROOT / 'target/release/attesor'
mutants = [
    ('continuation-bias', ROOT / 'crates/attesor/src/jit/mod.rs',
     'let cont = res.label_ip(&s.cont).expect("stub label") + prefix;',
     'let cont = res.label_ip(&s.cont).expect("stub label");'),
    ('probe-register-corruption', ROOT / 'crates/attesor/src/jit/emit.rs',
     'inc(qword_ptr(r15 + off::JIT_COST))', 'inc(qword_ptr(r15 + off::X))'),
]
originals = {path: path.read_text() for _, path, _, _ in mutants}
env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
env.update(ATTESOR_JIT_ON='jitcost,slotreuse', ATTESOR_CACHE=str(OUT / 'cache'),
           ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0')
def build(label):
    r = subprocess.run(['cargo', 'build', '--release', '-p', 'attesor'], cwd=ROOT,
                       text=True, capture_output=True, timeout=300)
    (OUT / (label + '-build.log')).write_text(r.stdout + r.stderr)
    assert r.returncode == 0, (label, r.stderr)
try:
    for label, path, old, new in mutants:
        assert originals[path].count(old) == 1, label
        path.write_text(originals[path].replace(old, new))
        build(label)
        r = subprocess.run([str(BIN), '--selftest-jit', 'jitcode', '256', '0x1234'],
                           cwd=ROOT, env=env, text=True, capture_output=True, timeout=60)
        text = r.stdout + r.stderr
        (OUT / (label + '.log')).write_text(text)
        assert 'MISMATCH jitcode' in text, (label, r.returncode, text)
        print('PASS sensitivity', label, next(s for s in text.splitlines() if 'jit selftest [jitcode]' in s), flush=True)
        path.write_text(originals[path])
finally:
    for path, original in originals.items(): path.write_text(original)
    build('restored')
for seed in ['1', '0x1234', '0xdeadbeef']:
    r = subprocess.run([str(BIN), '--selftest-jit', 'jitcode', '512', seed],
                       cwd=ROOT, env=env, text=True, capture_output=True, timeout=60)
    (OUT / ('restored-' + seed + '.log')).write_text(r.stdout + r.stderr)
    assert r.returncode == 0 and 'mismatch=0' in r.stdout, r.stdout + r.stderr
    print('PASS restored', r.stdout.strip(), flush=True)
