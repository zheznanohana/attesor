#!/usr/bin/env python3
"""Validate TBI mask reuse, including caller-saved clobbers and redirected fallback.
Performance timing belongs in compare-cpu.py, not in this correctness check.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def run(command, environment=None):
    r = subprocess.run([str(x) for x in command], text=True, capture_output=True,
                       timeout=120, env={**os.environ, **(environment or {})})
    if r.returncode:
        raise RuntimeError(f'{command}: exit {r.returncode}\n{r.stdout}\n{r.stderr}')
    return r.stdout


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('attesor')
    p.add_argument('--count', type=int, default=20000)
    args = p.parse_args()
    if args.count < 256:
        p.error('--count must cover at least 256 top-byte tags')
    binary = Path(args.attesor).resolve()
    for flag in ['', 'tbimask']:
        out = run([binary, '--selftest-jit', 'tbiblocks', args.count],
                  {'ATTESOR_JIT_OFF': flag})
        print(out.strip())
        counters = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', out)}
        assert counters.get('ok') == args.count and counters.get('mismatch') == 0, counters
        assert counters.get('skipped') == 0 and counters.get('native', 0) >= 14 * args.count, counters
        # The half FABS must actually enter the clobbering helper. The LD1 can
        # become native independently without weakening that required coverage.
        assert counters.get('fallback_insns', 0) >= args.count, counters
        assert counters['native'] + counters['fallback_insns'] == 16 * args.count, counters
    with tempfile.TemporaryDirectory(prefix='attesor-tbi-check-') as td:
        source = Path(__file__).with_name('tbi-chain.c')
        arm, x86 = Path(td) / 'arm64', Path(td) / 'x86'
        run(['clang', '-arch', 'arm64', '-O2', source, '-o', arm])
        run(['clang', '-arch', 'x86_64', '-O2', source, '-o', x86])
        reference = re.search(r'^checksum .+$', run([x86, 4097]), re.M).group(0)
        for label, env in [
            ('default', {'ATTESOR_JIT_OFF': ''}),
            ('old-shifts', {'ATTESOR_JIT_OFF': 'tbimask'}),
            ('redirected-commpage', {'ATTESOR_JIT_OFF': '', 'ATTESOR_NO_COMMPAGE': '1'}),
            ('host-call-return', {'ATTESOR_JIT_OFF': '', 'ATTESOR_JIT_ON': 'callret'}),
        ]:
            out = run([binary, '--no-aot', arm, 4097], env)
            checksum = re.search(r'^checksum .+$', out, re.M).group(0)
            assert checksum == reference, (label, checksum, reference)
            print(f'PASS {label}: {checksum}')


if __name__ == '__main__':
    main()
