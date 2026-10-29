#!/usr/bin/env python3
"""Serial cold-cache versus statically prewarmed short-process comparison.

Run alone after correctness tests. The synthetic fixture has one short integer
kernel and normal dyld/libc startup; this does not measure Electron or GUI ready
time. Static prewarm time is recorded separately and never runs the guest.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', required=True)
    parser.add_argument('--runs', type=int, default=7)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    if args.runs < 1:
        parser.error('--runs must be positive; use at least 5 for performance interpretation')
    binary = str(Path(args.attesor).resolve())
    source = Path(__file__).resolve().parents[1] / 'tests/attesor/aot-prewarm-probe.c'
    samples = []
    with tempfile.TemporaryDirectory(prefix='attesor-prewarm-bench-') as td:
        root = Path(td)
        guest, native = root / 'probe-arm64', root / 'probe-x86'
        for arch, exe in [('arm64', guest), ('x86_64', native)]:
            subprocess.run(['clang', '-arch', arch, '-O2', str(source), '-o', str(exe)], check=True)
        reference = subprocess.run([native], env={k: v for k, v in os.environ.items() if k != 'PREWARM_SENTINEL'},
                                   text=True, capture_output=True, check=True).stdout
        sentinel = root / 'guest-executed'
        env = {**os.environ, 'ATTESOR_STATS': '1', 'ATTESOR_NO_AOT_BUILD': '1',
               'ATTESOR_AOT_CHECKPOINT': '0', 'PREWARM_SENTINEL': str(sentinel)}
        for key in ['ATTESOR_GPU', 'ATTESOR_NATIVE_LIBS']:
            env.pop(key, None)
        # Keep caller-specified ISA/debug switches: the cache and both runtime
        # modes use exactly the same configuration, recorded in the report.
        warm = root / 'warm-cache'
        start = time.monotonic()
        prep = subprocess.run([binary, '--prewarm', str(guest)],
                              env={**env, 'ATTESOR_CACHE': str(warm)},
                              capture_output=True, text=True, check=True, timeout=300)
        prepare_ms = (time.monotonic() - start) * 1000
        if sentinel.exists() or prep.stdout:
            raise RuntimeError('prewarm unexpectedly executed guest code')
        before = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in warm.glob('*.aot')}
        if not any(k.startswith('main-') for k in before) or not any(k.startswith('dyld-') for k in before):
            raise RuntimeError('prewarm did not produce both image caches')
        for round_no in range(args.runs):
            for mode in (['cold', 'prewarmed'] if round_no % 2 == 0 else ['prewarmed', 'cold']):
                cache = warm if mode == 'prewarmed' else root / f'cold-{round_no}'
                sentinel.unlink(missing_ok=True)
                start = time.monotonic()
                r = subprocess.run([binary, str(guest)], env={**env, 'ATTESOR_CACHE': str(cache)},
                                   capture_output=True, text=True, check=True, timeout=120)
                wall_ms = (time.monotonic() - start) * 1000
                if r.stdout != reference or not sentinel.exists():
                    raise RuntimeError(f'{mode} semantic reference failed: {r.stdout!r}')
                loaded_main = bool(re.search(r'aot: loaded [1-9]\d* blocks for probe-arm64\n', r.stderr))
                loaded_dyld = bool(re.search(r'aot: loaded [1-9]\d* blocks for dyld\n', r.stderr))
                if (loaded_main, loaded_dyld) != ((True, True) if mode == 'prewarmed' else (False, False)):
                    raise RuntimeError(f'{mode} cache-load proof failed: {r.stderr}')
                samples.append(dict(round=round_no, mode=mode, wall_ms=wall_ms,
                                    stdout=r.stdout, stderr=r.stderr))
                print(f'{round_no + 1}/{args.runs} {mode}: {wall_ms:.2f} ms', flush=True)
        after = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in warm.glob('*.aot')}
        if after != before:
            raise RuntimeError('runtime changed the statically prepared translation files')
        medians = {mode: statistics.median(s['wall_ms'] for s in samples if s['mode'] == mode)
                   for mode in ['cold', 'prewarmed']}
        saved = medians['cold'] - medians['prewarmed']
        report = dict(binary=binary, sha256=hashlib.sha256(Path(binary).read_bytes()).hexdigest(),
                      runs=args.runs, prepare_ms=prepare_ms, prewarm_stderr=prep.stderr,
                      prewarm_files=before, samples=samples, medians_ms=medians,
                      time_reduction_percent=100 * saved / medians['cold'],
                      break_even_launches=math.ceil(prepare_ms / saved) if saved > 0 else None,
                      environment={k: v for k, v in env.items() if k.startswith('ATTESOR_')},
                      scope='synthetic short CLI process, including dyld/libc startup; not GUI or Electron')
        Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({k: report[k] for k in ['prepare_ms', 'medians_ms', 'time_reduction_percent', 'break_even_launches']}, indent=2))


if __name__ == '__main__':
    main()
