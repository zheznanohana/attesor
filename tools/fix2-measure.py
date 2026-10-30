#!/usr/bin/env python3
"""Serial, interleaved FIX2 timing. No tests/builds should run concurrently.

Production: --before ORIGINAL_BINARY --after FINAL_BINARY --guests DIR
File-backed test protocol: add --test-pool DIR (both binaries must be test builds).
All inherited ATTESOR_* switches are removed. Raw stdout/stderr retained in JSON.
"""
import argparse, json, os, pathlib, subprocess, time
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--before', required=True, type=pathlib.Path)
p.add_argument('--after', required=True, type=pathlib.Path)
p.add_argument('--guests', required=True, type=pathlib.Path)
p.add_argument('--out', required=True, type=pathlib.Path)
p.add_argument('--runs', type=int, default=3)
p.add_argument('--test-pool', type=pathlib.Path)
a = p.parse_args()
if a.runs < 3: p.error('at least three runs required')
a.out.parent.mkdir(parents=True, exist_ok=True)
rows = []
cases = [('before', 'off'), ('after', 'off'), ('before', 'rel32chain'),
         ('after', 'rel32chain'), ('before', 'sharedtrans'), ('after', 'sharedtrans')]
if a.test_pool:
    a.test_pool.mkdir(parents=True, exist_ok=True)
    cases = [(v, t) for v, t in cases if t != 'rel32chain']
for run in range(a.runs):
    # Rotate and reverse order: neither a feature nor a build always runs first.
    ordered = cases[run % len(cases):] + cases[:run % len(cases)]
    if run % 2: ordered = list(reversed(ordered))
    for guest in ['k-arm64', 'b-arm64']:
        for version, token in ordered:
            env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
            if token != 'off': env['ATTESOR_JIT_ON'] = token
            binary = getattr(a, version).resolve()
            cmd = [str(binary), '--no-aot', str((a.guests / guest).resolve())]
            if a.test_pool:
                env['ATTESOR_TEST_KERNEL'] = str((a.guests / guest).resolve())
                # Fresh pool for every producer measurement; quotas stay comparable.
                pool = a.test_pool / f'{version}-{token}-{guest}-{run}.pool'
                if pool.exists(): pool.unlink()
                env['ATTESOR_TEST_SHARED_FILE'] = str(pool.resolve())
                cmd = [str(binary), '--exact', 'fix2_kernel_worker', '--nocapture', '--test-threads=1']
            start = time.monotonic()
            r = subprocess.run(cmd, env=env, text=True, capture_output=True)
            row = dict(run=run+1, guest=guest, version=version, token=token,
                       command=cmd, seconds=time.monotonic()-start, rc=r.returncode,
                       stdout=r.stdout, stderr=r.stderr)
            rows.append(row)
            a.out.write_text(json.dumps(rows, indent=2) + '\n')
            print(f'{run+1} {guest} {version} {token} rc={r.returncode} {row["seconds"]:.3f}s', flush=True)
            if r.returncode: raise SystemExit(r.returncode)
