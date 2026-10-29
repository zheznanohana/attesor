#!/usr/bin/env python3
"""Cooperative cross-thread SMC plus ISB/IBTC regression, using only private guests.

Optional --baseline checks that the original binary fails the indirect test.
Timing is deliberately separate: run the generated probe with --bench N.
"""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('attesor')
    p.add_argument('--baseline')
    args = p.parse_args()
    binary = str(Path(args.attesor).resolve())
    source = Path(__file__).with_name('icache-handoff.c').resolve()
    with tempfile.TemporaryDirectory(prefix='attesor-icache-check-') as td:
        root = Path(td)
        guest, native = root / 'guest', root / 'native'
        for arch, path in [('arm64', guest), ('x86_64', native)]:
            subprocess.run(['clang', '-O2', '-arch', arch, str(source), '-o', str(path)], check=True)
        env = dict(os.environ, ATTESOR_CACHE=str(root / 'cache'), ATTESOR_NO_AOT_BUILD='1',
                   ATTESOR_AOT_CHECKPOINT='0')
        for key in ['ATTESOR_JIT_ON', 'ATTESOR_JIT_OFF', 'ATTESOR_JIT_DISABLE',
                    'ATTESOR_JIT_THRESHOLD', 'ATTESOR_NO_COMMPAGE']:
            env.pop(key, None)

        def run(command, extra=None):
            return subprocess.run([str(x) for x in command], env={**env, **(extra or {})},
                                  text=True, capture_output=True, timeout=60)

        if args.baseline:
            r = run([Path(args.baseline).resolve(), '--no-aot', guest, 'indirect', '2'])
            assert r.returncode == 1 and 'result=1 expected=2' in r.stderr, (r.returncode, r.stdout, r.stderr)
            print('PASS baseline demonstrates stale indirect target after legal publication', flush=True)
        for mode, extra, options in [
            ('default', {}, ['--no-aot']),
            ('immediate-jit', {'ATTESOR_JIT_THRESHOLD': '1'}, ['--no-aot']),
            ('callret', {'ATTESOR_JIT_ON': 'callret'}, ['--no-aot']),
            ('redirect', {'ATTESOR_NO_COMMPAGE': '1'}, ['--no-aot']),
            ('interpreter', {}, ['--interp', '--no-aot']),
        ]:
            for branch in ['direct', 'indirect']:
                r = run([binary, *options, guest, branch, '4'], extra)
                assert r.returncode == 0 and f'icache-handoff {branch} PASS rounds=4' in r.stdout, (mode, r.returncode, r.stdout, r.stderr)
                print(f'PASS {mode}: {r.stdout.strip()}', flush=True)
        ref = run([native, '--bench', '4097'])
        assert ref.returncode == 0, ref.stderr
        checksum = ref.stdout.splitlines()[-1]
        r = run([binary, '--no-aot', guest, '--bench', '4097'])
        assert r.returncode == 0 and r.stdout.splitlines()[-1] == checksum, (r.stdout, r.stderr)
        print('PASS ordinary-indirect workload agrees with independent native reference:', checksum)


if __name__ == '__main__':
    main()
