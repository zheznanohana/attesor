#!/usr/bin/env python3
"""Serial L1 matrix; private caches, exact result checks, resumable by binary hash.

Defaults: every opt-in alone, no opt-ins, all opt-ins, 100k per class and
three loop seeds. A passing process exit alone is NOT a self-test pass.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
TOKENS = (
    'avx512 avx512nan bgtrans bliccache callret fastenc flagreg hugepages '
    'ibtc2 isel2 jitcost loopcensus loopra2 nativefixups notbi offline-constprop '
    'offline-dse offline-layout-profile page16k pathcache preciseinvalidate '
    'process-qos regioncache rel32chain sharedtrans slotreuse tiered'
).split()
CLASSES = (
    'dpimm dpreg ldst branch blocks fpblocks fuse fuse2 selchain tbiblocks '
    'fpnan loops calls jitcode offline '
    '0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000'
).split()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--count', type=int, default=100000)
    parser.add_argument('--output', type=Path, default=ROOT/'target/integ1/l1')
    parser.add_argument('--configs', nargs='+', default=['default', *TOKENS, 'all'])
    parser.add_argument('--classes', nargs='*', default=CLASSES,
                        help='empty list runs only the requested cargo/AOT gates')
    parser.add_argument('--cargo', action='store_true', help='workspace tests for each configuration')
    parser.add_argument('--aot', action='store_true', help='loopra-aot-check for each configuration')
    parser.add_argument('--resume', action='store_true')
    args = parser.parse_args()
    if args.count <= 0:
        parser.error('--count must be positive')
    if args.aot and 'all-off' in args.configs:
        parser.error('loopra-aot-check owns its OFF toggles; all-off is only supported for cargo/self-tests')
    out = args.output.resolve()
    if not out.is_relative_to(ROOT/'target'):
        parser.error('--output must be under this worktree target/')
    out.mkdir(parents=True, exist_ok=True)
    binary = ROOT/'target/release/attesor'
    identity = hashlib.sha256(binary.read_bytes()).hexdigest()
    base = {k: v for k, v in os.environ.items()
            if not k.startswith(('ATTESOR_', 'DYLD_'))}
    base.update(TMPDIR='/private/tmp')
    results = []

    def run(label, kind, command, on, off='', cls=None, seed=None):
        file_label = label if len(label) < 100 else 'tokens-' + hashlib.sha256(label.encode()).hexdigest()[:12]
        name = f'{file_label}-{kind}' + (f'-{cls.replace(":", "-")}-{seed}' if cls else '')
        log, record = out/(name+'.log'), out/(name+'.json')
        key = dict(binary_sha256=identity, command=list(map(str, command)), on=on, off=off)
        if args.resume and record.exists() and (log.exists() or log.with_suffix('.log.gz').exists()):
            row = json.loads(record.read_text())
            if row.get('key') == key and row.get('status') == 'PASS':
                results.append(row)
                print('CACHED', name, flush=True)
                return
        cache = out/'cache'
        env = dict(base, ATTESOR_JIT_ON=on, ATTESOR_JIT_OFF=off,
                   ATTESOR_CACHE=str(cache), ATTESOR_NO_AOT_BUILD='1',
                   ATTESOR_AOT_CHECKPOINT='0')
        started = time.monotonic()
        timeout = False
        with log.open('w') as stream:
            try:
                rc = subprocess.run(command, cwd=ROOT, env=env, stdout=stream,
                                    stderr=subprocess.STDOUT, timeout=1800).returncode
            except subprocess.TimeoutExpired:
                rc, timeout = None, True
        content = log.read_text(errors='replace')
        passed = rc == 0
        lines = [line for line in content.splitlines() if 'jit selftest [' in line]
        if cls:
            passed &= (len(lines) == 1 and not re.search(r'\bMISMATCH\b', content)
                       and bool(re.search(r'\bmismatch=0\b', lines[0]))
                       and bool(re.search(r'\bok=[1-9]\d*\b', lines[0])))
        if kind == 'cargo':
            passed &= bool(re.search(r'test result: ok\. [1-9]\d* passed', content))
        if kind == 'aot':
            passed &= 'PASS loopra AOT:' in content
        row = dict(key=key, name=name, status='PASS' if passed else 'FAIL',
                   returncode=rc, timeout=timeout, seconds=round(time.monotonic()-started, 3),
                   result=lines or [line[:2000] for line in content.splitlines()[-5:]])
        record.write_text(json.dumps(row, indent=2)+'\n')
        results.append(row)
        print(row['status'], name, row['seconds'], '; '.join(row['result']), flush=True)
        # Keep logs/records, never duplicate the large system cache between cases.
        shutil.rmtree(cache, ignore_errors=True)

    for label in args.configs:
        on = '' if label == 'default' else ','.join(TOKENS) if label in ('all', 'all-off') else label
        off = on if label == 'all-off' else ''
        if args.cargo:
            run(label, 'cargo', ['cargo', 'test', '--release', '--workspace', '--', '--test-threads=1'], on, off)
        for cls in args.classes:
            for seed in (['0x1234', '0xdeadbeef', '0xc0ffee'] if cls == 'loops' else ['0x1234']):
                run(label, 'selftest', [str(binary), '--selftest-jit', cls, str(args.count), seed], on, off, cls, seed)
        if args.aot:
            run(label, 'aot', ['bash', 'tests/attesor/loopra-aot-check.sh', str(binary), on], on, off)
    (out/'summary.json').write_text(json.dumps(results, indent=2)+'\n')
    failed = [row['name'] for row in results if row['status'] != 'PASS']
    print(f'RESULT {len(results)-len(failed)} passed; {len(failed)} failed', flush=True)
    if failed:
        print('\n'.join(failed))
        raise SystemExit(1)


if __name__ == '__main__':
    main()
