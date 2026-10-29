#!/usr/bin/env python3
"""Alternating unchanged bench.c/kernels.c runs from verified private AOT caches."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', type=Path, default=Path('target/release/attesor'))
    p.add_argument('--directory', type=Path, default=Path('target/track-d/perf'))
    p.add_argument('--runs', type=int, default=3)
    a = p.parse_args()
    root = a.directory.resolve()
    root.mkdir(parents=True, exist_ok=True)
    binary = a.attesor.resolve()
    env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
    env.update(TMPDIR='/private/tmp', ATTESOR_NO_AOT_BUILD='1',
               ATTESOR_AOT_CHECKPOINT='0', ATTESOR_STATS='1')
    rows, caches = [], {}

    def run(label, args, extra):
        load = subprocess.check_output(['uptime'], text=True).strip()
        t = time.monotonic()
        r = subprocess.run([str(binary), *map(str, args)], env={**env, **extra},
                           text=True, capture_output=True, timeout=900, check=True)
        row = dict(label=label, uptime=load, seconds=time.monotonic()-t,
                   stdout=r.stdout, stderr=r.stderr)
        return row

    for name, source in [('bench', 'bench/bench.c'), ('kernels', 'bench/native-ratio/kernels.c')]:
        guest = root / name
        subprocess.run(['clang', '-O2', '-arch', 'arm64', source, '-o', str(guest)],
                       env=env, check=True)
        for flag in ['', 'offline-dse']:
            label = flag or 'baseline'
            cache = root / f'{name}-{label}'
            if cache.exists():
                raise RuntimeError(f'use a fresh measurement directory: {cache}')
            cache.mkdir()
            extra = dict(ATTESOR_CACHE=str(cache), ATTESOR_JIT_ON=flag)
            row = run(f'prepare-{name}-{label}', ['--prewarm', guest], extra)
            files = {}
            for f in sorted(cache.glob('*.aot')):
                data = f.read_bytes()
                files[f.name] = dict(bytes=len(data), blocks=struct.unpack_from('<Q', data, 16)[0],
                                     code_bytes=struct.unpack_from('<Q', data, 24)[0],
                                     sha256=hashlib.sha256(data).hexdigest())
            row['files'] = files
            rows.append(row)
            caches[name, flag] = extra, files
    def workload(label, name, flag):
        extra, files = caches[name, flag]
        row = run(label, [root/name], extra)
        assert re.search(r'aot: loaded [1-9]\d* blocks for '+name+r'\n', row['stderr']), row
        for f, info in files.items():
            assert hashlib.sha256((Path(extra['ATTESOR_CACHE'])/f).read_bytes()).hexdigest() == info['sha256']
        rows.append(row)
        print(json.dumps({k: v for k, v in row.items() if k != 'stderr'}), flush=True)
        (root/'results.json').write_text(json.dumps(rows, indent=2)+'\n')

    # Untimed for acceptance: materialize private DSC slide data before A/B,
    # rather than charging its first creation to the first measured process.
    for name in ['bench', 'kernels']:
        for flag in ['', 'offline-dse']:
            workload(f'warmup-{name}-{flag or "baseline"}', name, flag)
    for i in range(a.runs):
        for flag in ['', 'offline-dse']:
            for name in ['bench', 'kernels']:
                workload(f'run-{i+1}-{name}-{flag or "baseline"}', name, flag)

    (root/'results.json').write_text(json.dumps(rows, indent=2)+'\n')


if __name__ == '__main__':
    main()
