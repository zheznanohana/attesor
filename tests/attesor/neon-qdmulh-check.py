#!/usr/bin/env python3
"""Check QDMULH results, cumulative saturation, lane clearing and native coverage."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

root = Path(__file__).resolve().parents[2]
binary = Path(sys.argv[1] if len(sys.argv) > 1 else root / 'target/release/attesor').resolve()
count = int(os.environ.get('ATTESOR_NEON_COUNT', '20000'))

def run(command, **kwargs):
    r = subprocess.run([str(a) for a in command], capture_output=True, text=True,
                       timeout=240, check=True, **kwargs)
    return r.stdout.strip()

with tempfile.TemporaryDirectory(prefix='attesor-qdmulh-') as d:
    d = Path(d)
    arm, native = d / 'arm64', d / 'x86'
    for arch, output in [('arm64', arm), ('x86_64', native)]:
        run(['clang', '-O2', '-arch', arch, root / 'tests/attesor/neon-qdmulh.c', '-o', output])
    expected = run([native])
    assert expected == 'qdmulh digest 4d92fd0ec98359f5 qc_cases 12708 boundary PASS', expected
    for mode in [[], ['--interp']]:
        result = run([binary, '--no-aot', *mode, arm])
        assert result == expected, (mode, expected, result)
        print('PASS', 'interpreter' if mode else 'JIT', result, flush=True)
    for mask in ['9fe0fc00:0e60b400', '9fe0fc00:0ea0b400']:
        result = run([binary, '--selftest-jit', mask, count])
        print(result, flush=True)
        stats = dict((k, int(v)) for k, v in re.findall(r'(\w+)=(\d+)', result.splitlines()[-1]))
        assert stats.get('mismatch') == 0 and stats.get('skipped') == 0, stats
        assert stats.get('native') == count and stats.get('ok') == count, stats
    expected = run([native, '--bench', '257'])
    for env in [os.environ.copy(), dict(os.environ, ATTESOR_JIT_OFF='qdmulh')]:
        result = run([binary, '--no-aot', arm, '--bench', '257'], env=env)
        assert result == expected, (expected, result)
    print('PASS benchmark checksum', expected, flush=True)
