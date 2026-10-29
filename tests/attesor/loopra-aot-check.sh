#!/bin/bash
# Native/interpreter/JIT/cold and warm private AOT differential checks.
set -euo pipefail
cd "$(dirname "$0")/../.."
ATTESOR=${1:-target/release/attesor}
# Explicit temp root also works in the restricted development sandbox.
export TMPDIR=/private/tmp
python3 - "$ATTESOR" <<'PY'
import os, re, subprocess, sys, tempfile
from pathlib import Path
binary = Path(sys.argv[1]).resolve()
clean = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
def run(cmd, extra=None):
    return subprocess.run([str(x) for x in cmd], env={**clean, **(extra or {})},
                          text=True, capture_output=True, check=True, timeout=300)
with tempfile.TemporaryDirectory(prefix='loopra-aot-') as td:
    root = Path(td)
    for k, name in enumerate(['int-hash', 'crc32', 'sieve', 'branchy', 'pointer', 'pre-post', 'ldp-stp']):
        guest, native, cache = root / name, root / (name + '-x86'), root / (name + '-cache')
        cache.mkdir()
        for arch, dest in [('arm64', guest), ('x86_64', native)]:
            run(['clang', '-O2', '-arch', arch, f'-DKERNEL={k}', 'tests/attesor/loopra-aot.c', '-o', dest])
        expected = run([native]).stdout.strip()
        env = {'ATTESOR_CACHE': str(cache), 'ATTESOR_AOT_CHECKPOINT': '0',
               'ATTESOR_AOT_SYNC': '1', 'ATTESOR_STATS': '1'}
        for label, opts, extra in [('interp', ['--interp'], {}), ('cold-JIT', [], env)]:
            p = run([binary, *opts, guest], {**env, **extra})
            assert p.stdout.strip() == expected, (name, label, p.stdout, expected)
            assert not re.search(r'aot: loaded [1-9]', p.stderr), p.stderr
        files = list(cache.glob('main-*.aot'))
        assert len(files) == 1, list(cache.iterdir())
        default_header = files[0].read_bytes()[:40]
        # Only the main image may supply AOT code in the warm run. Thus a
        # positive region counter proves this workload ran a disk-loaded region.
        for f in cache.glob('*.aot'):
            if f != files[0]: f.unlink()
        warm_env = {**env, 'ATTESOR_NO_AOT_BUILD': '1'}
        p = run([binary, guest], warm_env)
        assert p.stdout.strip() == expected, (name, p.stdout, expected)
        loaded = re.search(r'aot: loaded ([1-9]\d*) blocks for ' + re.escape(name) + r'(?:\s|$)', p.stderr)
        regions = re.search(r'aot: loop_regions=([1-9]\d*)', p.stderr)
        assert loaded and regions, (name, p.stderr)
        assert len(re.findall(r'aot: loaded ', p.stderr)) == 1, p.stderr
        print(f'PASS {name}: native=interp=cold-JIT=warm-AOT {expected}; blocks={loaded[1]} aot_loop_regions={regions[1]}', flush=True)
        # OFF must reject the on-cache; then synchronously build a new one.
        off = {**env, 'ATTESOR_JIT_OFF': 'loopra'}
        p = run([binary, guest], off)
        assert p.stdout.strip() == expected and not re.search(r'aot: loaded [1-9]', p.stderr), p.stderr
        off_header = files[0].read_bytes()[:40]
        assert default_header[8:12] != off_header[8:12], 'loopra did not change AOT identity'
        p = run([binary, guest], {**off, 'ATTESOR_NO_AOT_BUILD': '1'})
        assert p.stdout.strip() == expected and re.search(r'aot: loaded [1-9]', p.stderr), p.stderr
        assert re.search(r'aot: loop_regions=0(?:\s|$)', p.stderr), p.stderr
        print(f'PASS {name}: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0', flush=True)
print('PASS loopra AOT: all 7 workloads and cache identities match')
PY
