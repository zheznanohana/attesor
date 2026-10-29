#!/usr/bin/env python3
"""Stress pthread creation/join and verify results against native x86.

The guest's four trivial workers can exit immediately. A join must never observe
an uninitialized thread, reclaim its allocation before host setup, or lose its
return value. Every run uses a private process and build directory.
"""
import argparse
from pathlib import Path
import subprocess
import tempfile


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('attesor')
    p.add_argument('--runs', type=int, default=10)
    p.add_argument('--rounds', type=int, default=30)
    args = p.parse_args()
    if min(args.runs, args.rounds) < 1:
        p.error('runs and rounds must be positive')
    attesor = str(Path(args.attesor).resolve())
    source = Path(__file__).with_name('thread-startup.c')
    expected = f'thread-startup PASS {args.rounds}\n'
    with tempfile.TemporaryDirectory(prefix='attesor-thread-startup-') as td:
        for arch in ['x86_64', 'arm64']:
            exe = str(Path(td) / arch)
            subprocess.run(['clang', '-arch', arch, '-O2', str(source), '-o', exe], check=True)
            for i in range(1 if arch == 'x86_64' else args.runs):
                command = [exe, str(args.rounds)] if arch == 'x86_64' else [attesor, '--no-aot', exe, str(args.rounds)]
                r = subprocess.run(command, text=True, capture_output=True, timeout=60)
                if r.returncode or r.stdout != expected:
                    raise RuntimeError(f'{arch} run {i + 1}: rc={r.returncode}\n{r.stdout}\n{r.stderr}')
            print(f'PASS {arch}: {args.rounds} rounds x 4 joined workers x '
                  f'{1 if arch == "x86_64" else args.runs} processes')


if __name__ == '__main__':
    main()
