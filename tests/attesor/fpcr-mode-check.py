#!/usr/bin/env python3
"""Real guest FPCR write/FSQRT gates in interpreter, JIT, region JIT and AOT.

All binaries and caches are temporary. No inherited ATTESOR_* configuration or
user cache is consumed. AOT execution requires positive main-image load evidence.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import tempfile

EXPECTED = 'fpcr-boundary PASS cases=20 checksum=da55d2f630897cc0\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', required=True)
    args = parser.parse_args()
    binary = Path(args.attesor).resolve()
    clean = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}

    with tempfile.TemporaryDirectory(prefix='attesor-fpcr-mode-') as td:
        root = Path(td)
        guest = root / 'fpcr-mode'
        cache = root / 'cache'
        cache.mkdir()
        env = {**clean, 'ATTESOR_CACHE': str(cache), 'ATTESOR_NO_AOT_BUILD': '1',
               'ATTESOR_AOT_CHECKPOINT': '0', 'ATTESOR_STATS': '1'}

        def run(argv, extra=None):
            result = subprocess.run([str(v) for v in argv], capture_output=True,
                                    text=True, timeout=300,
                                    env={**env, **(extra or {})})
            if result.returncode:
                raise AssertionError(f'{argv}: exit {result.returncode}\n'
                                     f'{result.stdout}\n{result.stderr}')
            return result

        run(['clang', '-arch', 'arm64', '-O2',
             Path(__file__).with_name('fpcr-mode.c'), '-o', guest])
        loaded = r'aot: loaded ([1-9]\d*) blocks for fpcr-mode(?:\s|$)'
        for label, options, extra in [
            ('interpreter', ['--no-aot', '--interp'], {}),
            ('JIT', ['--no-aot'], {}),
            ('region JIT', ['--no-aot'], {'ATTESOR_JIT_ON': 'regioncache'}),
        ]:
            result = run([binary, *options, guest], extra)
            assert result.stdout == EXPECTED, (label, result.stdout, result.stderr)
            assert not re.search(loaded, result.stderr), result.stderr
            print('PASS', label, result.stdout.strip(), flush=True)

        assert not list(cache.glob('main-*.aot')), list(cache.iterdir())
        region = {'ATTESOR_JIT_ON': 'regioncache'}
        run([binary, '--prewarm', guest], region)
        artifacts = list(cache.glob('main-*.aot'))
        assert len(artifacts) == 1, list(cache.iterdir())
        data = artifacts[0].read_bytes()
        assert len(data) >= 40 and int.from_bytes(data[16:24], 'little') > 0
        result = run([binary, guest], region)
        assert result.stdout == EXPECTED, (result.stdout, result.stderr)
        evidence = re.search(loaded, result.stderr)
        assert evidence, result.stderr
        print('PASS region AOT', evidence.group(0).strip(),
              result.stdout.strip(), flush=True)


if __name__ == '__main__':
    main()
