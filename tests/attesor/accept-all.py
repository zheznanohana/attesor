#!/usr/bin/env python3
"""Aggregate automated acceptance for the eight optimization workstreams.

Performance runs are separate. This gate covers CPU semantics, persistent cache,
debugger transport, native-library calls and isolated launcher fixtures. It does
not certify arbitrary applications, real Finder/Dock rendering or hardware video.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', default='target/release/attesor')
    p.add_argument('--output', required=True)
    p.add_argument('--count', type=int, default=100000)
    args = p.parse_args()
    root = Path(__file__).resolve().parents[2]
    binary = Path(args.attesor).resolve()
    output = Path(args.output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    cpu_output = output.with_suffix('.cpu.json')
    jobs = [
        ('workspace-unit-tests', ['cargo', 'test', '--workspace', '--', '--test-threads=1']),
        ('cpu-semantics', ['python3', 'tests/attesor/accept.py', '--attesor', str(binary),
                           '--count', str(args.count), '--output', str(cpu_output)]),
        ('crypto', ['env', 'ATTESOR=' + str(binary), 'tests/attesor/crypto-check.sh']),
        ('aot-persistence', ['python3', 'tests/attesor/aot-checkpoint.py', '--attesor', str(binary)]),
        ('neon-saturation', ['python3', 'tests/attesor/neon-qdmulh-check.py', str(binary)]),
        ('fp16-conversion', ['python3', 'tests/attesor/fp16-convert-check.py', str(binary)]),
        ('fpcr-mode-transitions', ['python3', 'tests/attesor/fpcr-mode-check.py', '--attesor', str(binary)]),
        ('debugger-protocol', ['python3', 'tests/attesor/gdb-protocol.py', str(binary)]),
        ('debugger-lldb', ['python3', 'tests/attesor/gdb-lldb-fd.py', str(binary)]),
        ('native-zlib', ['env', 'ATTESOR=' + str(binary), 'tests/attesor/native-zlib-check.sh']),
        ('thread-startup', ['python3', 'tests/attesor/thread-startup-check.py', str(binary)]),
        ('launcher-fixtures', ['python3', 'tests/attesor/launcher-check.py', '--attesor', str(binary)]),
        ('startup-driver', ['python3', 'tests/attesor/startup-driver-check.py']),
        ('exec-modes-and-runtime-options', ['python3', 'tests/attesor/exec-modes-check.py', str(binary)]),
        ('aot-static-prewarm', ['python3', 'tests/attesor/aot-prewarm.py', '--attesor', str(binary)]),
        ('tbi-mask', ['python3', 'tests/attesor/tbi-mask-check.py', str(binary)]),
        ('direct-test-branches', ['python3', 'tests/attesor/direct-branch-check.py', str(binary)]),
        ('register-next-use', ['python3', 'tests/attesor/nextuse-check.py', str(binary)]),
        ('natural-loop-cache', ['python3', 'tests/attesor/regioncache-check.py', str(binary)]),
        ('fp16-exception-state', ['python3', 'tests/attesor/fp16-status-check.py', str(binary)]),
        ('icache-thread-handoff', ['python3', 'tests/attesor/icache-handoff-check.py', str(binary)]),
    ]
    results = []
    for label, command in jobs:
        start = time.monotonic()
        try:
            r = subprocess.run(command, cwd=root, text=True, capture_output=True, timeout=1200)
            row = dict(name=label, command=command, returncode=r.returncode,
                       stdout=r.stdout, stderr=r.stderr, passed=r.returncode == 0)
        except subprocess.TimeoutExpired as e:
            row = dict(name=label, command=command, passed=False, timeout=True,
                       stdout=str(e.stdout or ''), stderr=str(e.stderr or ''))
        row['seconds'] = time.monotonic() - start
        results.append(row)
        print(f'{label}: {"PASS" if row["passed"] else "FAIL"} ({row["seconds"]:.1f}s)', flush=True)
        # Persist partial results as well, so a failing gate is reviewable.
        report = dict(binary=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                      automated_passed=len(results) == len(jobs) and all(item['passed'] for item in results),
                      completed=len(results), required=len(jobs), results=results,
                      unverified=['arbitrary application compatibility', 'real Finder/Dock UI',
                                  'hardware VideoToolbox', 'whole-application startup speedup'])
        output.write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['automated_passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
