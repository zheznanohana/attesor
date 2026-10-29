#!/usr/bin/env python3
"""Opt-in write-through natural-loop gates; no performance timing."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def run(cmd, extra=None):
    p = subprocess.run([str(v) for v in cmd], capture_output=True, text=True,
                       check=True, timeout=300, env={**os.environ, **(extra or {})})
    return p.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('attesor')
    parser.add_argument('--count', type=int, default=10000)
    args = parser.parse_args()
    binary = Path(args.attesor).resolve()
    failures = []
    # General differential suites exercise all unchanged classes with opt-in set.
    for cls in ['blocks', 'fpblocks', 'tbiblocks', 'selchain']:
        out = run([binary, '--selftest-jit', cls, args.count], {'ATTESOR_JIT_ON': 'regioncache'})
        fields = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', out)}
        if fields.get('mismatch') != 0 or fields.get('ok', 0) <= 0:
            failures.append(cls)
        print(out.strip(), flush=True)
    with tempfile.TemporaryDirectory(prefix='attesor-regioncache-') as td:
        root = Path(td)
        src = Path(__file__).with_name('regioncache.c')
        native, guest = root / 'x86', root / 'arm64'
        for arch, exe in [('arm64', guest), ('x86_64', native)]:
            run(['clang', '-arch', arch, '-O2', src, '-o', exe])
        reference = run([native, 257]).strip()
        for label, opts, env in [
            ('baseline JIT', ['--no-aot'], {'ATTESOR_JIT_ON': ''}),
            ('region JIT', ['--no-aot'], {'ATTESOR_JIT_ON': 'regioncache'}),
            ('region commpage', ['--no-aot'], {'ATTESOR_JIT_ON': 'regioncache', 'ATTESOR_NO_COMMPAGE': '1'}),
            ('region old predictor', ['--no-aot'], {'ATTESOR_JIT_ON': 'regioncache', 'ATTESOR_JIT_OFF': 'nextuse'}),
            ('region AOT cold', [], {'ATTESOR_JIT_ON': 'regioncache'}),
            ('region AOT warm', [], {'ATTESOR_JIT_ON': 'regioncache'}),
        ]:
            out = run([binary, *opts, guest, 257], env)
            actual = re.search(r'^checksum .+$', out, re.M).group(0)
            assert actual == reference, (label, actual, reference)
            print('PASS', label, actual, flush=True)
    assert not failures, 'random differential gates failed: ' + ', '.join(failures)


if __name__ == '__main__':
    main()
