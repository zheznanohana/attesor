#!/usr/bin/env python3
"""Native/interpreter/JIT/disk-AOT parity, real optimizer work, immutable blobs and identity gates."""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', type=Path, default=Path('target/release/attesor'))
    args = parser.parse_args()
    binary = args.attesor.resolve()
    clean = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
    with tempfile.TemporaryDirectory(prefix='offline-check-', dir='/private/tmp') as td:
        root = Path(td)
        guest, native, cache = root/'probe', root/'native', root/'cache'
        env = dict(clean, TMPDIR='/private/tmp', ATTESOR_CACHE=str(cache),
                   ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0', ATTESOR_STATS='1')

        def run(argv, flag=''):
            r = subprocess.run(list(map(str, argv)), env={**env, 'ATTESOR_JIT_ON': flag},
                               text=True, capture_output=True, check=True, timeout=300)
            return r

        for arch, dest in [('arm64', guest), ('x86_64', native)]:
            run(['clang', '-O2', '-arch', arch, Path(__file__).with_name('offline.c'), '-o', dest])
        expected = run([native]).stdout
        assert run([binary, '--interp', guest]).stdout == expected
        for flag in ['', 'offline-dse']:
            assert run([binary, '--no-aot', guest], flag).stdout == expected
        print('PASS native = interpreter = default JIT = flagged JIT', expected.strip(), flush=True)
        build = run([binary, '--prewarm', guest], 'offline-dse')
        removed = re.search(r'offline-dse: '+re.escape(str(guest))+r' host_insns=(\d+) removed_stores=([1-9]\d*)', build.stderr)
        assert removed, build.stderr
        data = {p.name: p.read_bytes() for p in cache.glob('*.aot')}
        assert data
        for _ in range(2):
            r = run([binary, guest], 'offline-dse')
            assert r.stdout == expected
            assert re.search(r'aot: loaded [1-9]\d* blocks for probe\n', r.stderr), r.stderr
            assert not re.search(r'offline-dse:', r.stderr), 'warm run must not rebuild'
            assert {p.name: p.read_bytes() for p in cache.glob('*.aot')} == data
        print(f'PASS serialized optimized cache reloads twice: removed_stores={removed[2]}; blobs unchanged', flush=True)
        assert run([binary, guest]).stdout == expected
        assert not re.search(r'aot: loaded [1-9]', run([binary, guest]).stderr)
        print('PASS default rejects opt-in cache', flush=True)
        optimized_header = next(v[:40] for k, v in data.items() if k.startswith('main-'))
        run([binary, '--prewarm', guest])
        default_header = next(cache.glob('main-*.aot')).read_bytes()[:40]
        assert optimized_header[8:12] != default_header[8:12]
        rejected = run([binary, guest], 'offline-dse')
        assert rejected.stdout == expected and not re.search(r'aot: loaded [1-9]', rejected.stderr)
        accepted = run([binary, guest])
        assert accepted.stdout == expected and re.search(r'aot: loaded [1-9]\d* blocks for probe\n', accepted.stderr)
        print('PASS opt-in rejects default cache; default loads its own distinct identity', flush=True)


if __name__ == '__main__':
    main()
