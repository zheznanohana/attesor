#!/usr/bin/env python3
"""Validate FPCR-sensitive half conversions against a separate integer reference."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[2]
attesor = Path(sys.argv[1] if len(sys.argv) > 1 else root / 'target/release/attesor').resolve()


def run(command, **kwargs):
    result = subprocess.run([str(x) for x in command], cwd=root, capture_output=True,
                            text=True, timeout=120, check=True, **kwargs)
    return result.stdout.strip()


with tempfile.TemporaryDirectory(prefix='attesor-fp16-') as tmp:
    tmp = Path(tmp)
    outputs = {}
    for arch in ['x86_64', 'arm64']:
        binary = tmp / arch
        run(['clang', '-arch', arch, '-O2', root / 'tests/attesor/fp16-convert.c', '-o', binary])
        if arch == 'x86_64':
            outputs['integer-reference'] = run([binary])
        else:
            outputs['JIT'] = run([attesor, '--no-aot', binary])
            outputs['interpreter'] = run([attesor, '--interp', '--no-aot', binary])
    if len(set(outputs.values())) != 1 or 'PASS cases=60000' not in outputs['JIT']:
        raise SystemExit(f'FAIL output comparison: {outputs}')
    for mode, output in outputs.items():
        print(f'PASS {mode}: {output}', flush=True)
    for fpcr in [False, True]:
        env = dict(os.environ)
        if fpcr:
            env['ATTESOR_SELFTEST_FPCR'] = '1'
        else:
            env.pop('ATTESOR_SELFTEST_FPCR', None)
        output = run([attesor, '--selftest-jit', 'bffffc00:0e216800', '20000'], env=env)
        counters = {k: int(v) for k, v in re.findall(r'(ok|mismatch|skipped|native)=(\d+)', output)}
        if counters != {'ok': 20000, 'mismatch': 0, 'skipped': 0, 'native': 20000}:
            raise SystemExit(f'FAIL native coverage: {output}')
        print(('FPCR ' if fpcr else 'default ') + output, flush=True)
