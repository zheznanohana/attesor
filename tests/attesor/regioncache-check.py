#!/usr/bin/env python3
"""Opt-in write-through natural-loop gates; no performance timing."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def process(cmd, extra=None):
    p = subprocess.run([str(v) for v in cmd], capture_output=True, text=True,
                       check=True, timeout=300, env={**{k: v for k, v in os.environ.items() if not k.startswith("ATTESOR_")},
                                      **(extra or {})})
    return p


def run(cmd, extra=None):
    return process(cmd, extra).stdout


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
        ]:
            out = run([binary, *opts, guest, 257], env)
            actual = re.search(r'^checksum .+$', out, re.M).group(0)
            assert actual == reference, (label, actual, reference)
            print('PASS', label, actual, flush=True)
        # AOT evidence must belong to this test. Disable background builders so
        # cold/warm cannot race a checkpoint or consume the user's cache.
        cache = root / 'cache'
        cache.mkdir()
        aot_env = {'ATTESOR_JIT_ON': 'regioncache', 'ATTESOR_CACHE': str(cache),
                   'ATTESOR_NO_AOT_BUILD': '1', 'ATTESOR_AOT_CHECKPOINT': '0',
                   'ATTESOR_STATS': '1'}
        loaded = r'aot: loaded ([1-9]\d*) blocks for arm64(?:\s|$)'
        cold = process([binary, guest, 257], aot_env)
        assert cold.stdout.strip() == reference, cold.stdout
        assert not re.search(loaded, cold.stderr), cold.stderr
        assert not list(cache.glob('main-*.aot')), list(cache.iterdir())
        print('PASS region AOT cold: empty private cache, no load', flush=True)
        process([binary, '--prewarm', guest], aot_env)
        files = list(cache.glob('main-*.aot'))
        assert len(files) == 1, list(cache.iterdir())
        data = files[0].read_bytes()
        assert len(data) >= 40 and int.from_bytes(data[16:24], 'little') > 0
        for label in ['prewarmed AOT load', 'warm AOT reload']:
            warm = process([binary, guest, 257], aot_env)
            assert warm.stdout.strip() == reference, warm.stdout
            evidence = re.search(loaded, warm.stderr)
            assert evidence, warm.stderr
            print('PASS region', label, evidence.group(0), reference, flush=True)
        changed = process([binary, guest, 257], {**aot_env, 'ATTESOR_JIT_ON': ''})
        assert changed.stdout.strip() == reference, changed.stdout
        assert not re.search(loaded, changed.stderr), changed.stderr
        print('PASS region AOT identity: baseline rejects opt-in cache', flush=True)
    assert not failures, 'random differential gates failed: ' + ', '.join(failures)


if __name__ == '__main__':
    main()
