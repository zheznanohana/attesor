#!/usr/bin/env python3
"""Register-cache prediction gates; execute separately from performance runs."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def run(cmd, extra=None):
    p = subprocess.run([str(x) for x in cmd], capture_output=True, text=True,
                       check=True, timeout=300, env={**os.environ, **(extra or {})})
    return p.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('attesor')
    parser.add_argument('--count', type=int, default=100000)
    args = parser.parse_args()
    binary = Path(args.attesor).resolve()
    for mode in ['', 'nextuse']:
        env = {'ATTESOR_JIT_OFF': mode}
        for cls in ['blocks', 'fpblocks']:
            out = run([binary, '--selftest-jit', cls, args.count], env)
            fields = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', out)}
            assert fields.get('mismatch') == 0 and fields.get('ok', 0) > 0, out
            assert fields.get('insns', 0) > fields.get('fallback_insns', 0), out
            if cls == 'fpblocks':
                assert fields['ok'] == args.count and fields['fallback_insns'] == 0, out
            print(('next-use' if not mode else 'old-policy'), out.strip(), flush=True)
        out = run([binary, '--selftest-jit', 'tbiblocks', 5000], env)
        fields = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', out)}
        assert fields.get('ok') == 5000 and fields.get('mismatch') == 0, out
        assert fields.get('native', 0) >= 14 * 5000 and fields.get('fallback_insns', 0) >= 5000, out
        print(('next-use' if not mode else 'old-policy'), out.strip(), flush=True)
        out = run([binary, '--selftest-jit', 'selchain', 20000], env)
        fields = {k: int(v) for k, v in re.findall(r'(\w+)=(\d+)', out)}
        assert fields.get('mismatch') == 0 and fields.get('native', 0) > 0, out
        print(('next-use' if not mode else 'old-policy'), out.strip(), flush=True)
    with tempfile.TemporaryDirectory(prefix='attesor-nextuse-check-') as td:
        root = Path(td)
        src = Path(__file__).with_name('nextuse-pressure.c')
        guest, native = root / 'arm64', root / 'x86'
        for arch, exe in [('arm64', guest), ('x86_64', native)]:
            run(['clang', '-arch', arch, '-O2', '-ffp-contract=off', src, '-o', exe])
        reference = re.search(r'^checksum .+$', run([native, 257]), re.M).group(0)
        for label, env in [('next-use', {'ATTESOR_JIT_OFF': ''}),
                           ('old-policy', {'ATTESOR_JIT_OFF': 'nextuse'}),
                           ('redirected', {'ATTESOR_JIT_OFF': '', 'ATTESOR_NO_COMMPAGE': '1'}),
                           ('call-return', {'ATTESOR_JIT_OFF': '', 'ATTESOR_JIT_ON': 'callret'})]:
            out = run([binary, '--no-aot', guest, 257], env)
            actual = re.search(r'^checksum .+$', out, re.M).group(0)
            assert actual == reference, (label, out, reference)
            print('PASS', label, actual, flush=True)
        # Exceptional-value recurrence stresses eviction with more V registers
        # than cache slots. ARM interpreter supplies exact ARM NaN priority.
        ref = re.search(r'^checksum .+$', run([binary, '--no-aot', '--interp', guest, 257, 'exceptional']), re.M).group(0)
        for mode in ['', 'nextuse']:
            actual = re.search(r'^checksum .+$', run([binary, '--no-aot', guest, 257, 'exceptional'],
                               {'ATTESOR_JIT_OFF': mode}), re.M).group(0)
            assert actual == ref, (mode, actual, ref)
            print('PASS exceptional V pressure', mode or 'next-use', actual, flush=True)



if __name__ == '__main__':
    main()
