#!/usr/bin/env python3
"""Check CBZ/CBNZ and TBZ/TBNZ against native decisions and preserved NZCV.
This is correctness validation; run performance comparisons separately.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def run(command, environment=None):
    r = subprocess.run([str(a) for a in command], text=True, capture_output=True,
                       timeout=180, env={**os.environ, **(environment or {})})
    if r.returncode:
        raise RuntimeError(f'{command}: exit {r.returncode}\n{r.stdout}\n{r.stderr}')
    return r.stdout


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('attesor')
    p.add_argument('--count', type=int, default=20000)
    args = p.parse_args()
    if args.count < 1000:
        p.error('--count must be at least 1000')
    binary = Path(args.attesor).resolve()
    source = Path(__file__).with_name('direct-branch.c')
    with tempfile.TemporaryDirectory(prefix='attesor-direct-branch-') as td:
        arm, x86 = Path(td) / 'arm64', Path(td) / 'x86'
        for arch, path in [('arm64', arm), ('x86_64', x86)]:
            run(['clang', '-arch', arch, '-O2', source, '-o', path])
        reference = run([x86]).strip()
        assert 'PASS cases=196608' in reference
        checksum = re.search(r'^checksum .+$', run([x86, '--bench', 4097]), re.M).group(0)
        for label, switch in [('direct-jcc', ''), ('setcc-test-baseline', 'directbranch')]:
            env = {'ATTESOR_JIT_OFF': switch}
            out = run([binary, '--no-aot', arm], env).strip()
            assert out == reference, (label, out, reference)
            print(f'PASS {label}: {out}', flush=True)
            out = run([binary, '--no-aot', arm, '--bench', 4097], env)
            assert re.search(r'^checksum .+$', out, re.M).group(0) == checksum, (label, out)
            print(f'PASS {label}: throughput workload {checksum}', flush=True)
            out = run([binary, '--selftest-jit', 'branch', args.count], env)
            counters = dict((k, int(v)) for k, v in re.findall(r'(\w+)=(\d+)', out))
            assert counters.get('mismatch') == 0 and counters.get('native', 0) > 0, counters
            assert counters.get('ok', 0) > 0, counters
            print(label, out.strip(), flush=True)


if __name__ == '__main__':
    main()
