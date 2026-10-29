#!/usr/bin/env python3
"""Run the integrated CPU acceptance gate; retain every command and result.

Usage: python3 tests/attesor/accept.py --attesor target/release/attesor \
  --output /tmp/attesor-acceptance.json

This is a correctness gate, not a performance measurement or a claim of universal
application compatibility. Timing must be measured separately without this job.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--count', type=int, default=100000)
    args = parser.parse_args()
    if args.count < 1000:
        parser.error('--count must be at least 1000 for useful coverage')
    binary = Path(args.attesor).resolve()
    root = Path(__file__).resolve().parents[2]
    results = []

    def run(label, command, timeout=240, environment=None):
        start = time.monotonic()
        child = subprocess.Popen([str(a) for a in command], cwd=root,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 text=True, start_new_session=True,
                                 env={**os.environ, **(environment or {})})
        expired = False
        try:
            out, err = child.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            expired = True
            os.killpg(child.pid, signal.SIGKILL)
            out, err = child.communicate()
        row = dict(name=label, command=[str(a) for a in command],
                   returncode=child.returncode, timeout=expired,
                   environment=environment or {},
                   stdout=out, stderr=err, seconds=time.monotonic() - start)
        row['passed'] = child.returncode == 0 and not expired
        results.append(row)
        return row

    with tempfile.TemporaryDirectory(prefix='attesor-accept-') as scratch:
        scratch = Path(scratch)
        for name in ['fpalias', 'crypto']:
            outputs = []
            for arch in ['x86_64', 'arm64']:
                exe = scratch / (name + '-' + arch)
                build = run('build-' + name + '-' + arch,
                            ['clang', '-O2', '-ffp-contract=off', '-arch', arch,
                             root / 'tests/attesor' / (name + '.c'), '-o', exe])
                if not build['passed']:
                    break
                command = [exe] if arch == 'x86_64' else [binary, '--no-aot', exe]
                result = run(name + '-' + arch, command)
                if name == 'crypto':
                    digest = re.search(r'digest ([0-9a-f]{64})', result['stdout'])
                    outputs.append(digest.group(1) if digest else None)
                else:
                    outputs.append(result['stdout'].strip())
            passed = len(outputs) == 2 and all(outputs) and outputs[0] == outputs[1]
            results.append(dict(name=name + '-native-comparison', passed=bool(passed),
                                outputs=outputs))
        classes = [(c, args.count, {}) for c in
                   ['fpblocks', 'blocks', 'selchain', 'ff000000:1f000000',
                    'bf20fc00:0e20cc00', '9f800400:0f000000']]
        classes += [('fpnan', 62208, {}),
                    ('fpnan', 62208, {'ATTESOR_SELFTEST_FPCR': '1'})]
        for cls, count, environment in classes:
            suffix = '-fpcr' if environment else ''
            row = run('selftest-' + cls + suffix,
                      [binary, '--selftest-jit', cls, count], environment=environment)
            summary = re.search(r'jit selftest .*: (.*)', row['stdout'])
            counters = dict((k, int(v)) for k, v in
                            re.findall(r'(\w+)=(\d+)', summary.group(1))) if summary else {}
            row['counters'] = counters
            # Random integer blocks include instructions not yet implemented in
            # the JIT; insist on native coverage, but do not confuse that with
            # complete instruction coverage. The focused FP generator needs none.
            native_coverage = (counters.get('native', 0) > 0 or
                               counters.get('insns', 0) > counters.get('fallback_insns', 0))
            if cls == 'fpblocks':
                native_coverage = native_coverage and counters.get('fallback_insns') == 0
            row['passed'] = (row['passed'] and counters.get('mismatch') == 0
                             and counters.get('ok', 0) > 0
                             and native_coverage)
            print(f'{row["name"]}: {"PASS" if row["passed"] else "FAIL"} {counters}', flush=True)
    report = dict(binary=str(binary),
                  sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  passed=all(r['passed'] for r in results), results=results)
    Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
    print(f'Acceptance: {"PASS" if report["passed"] else "FAIL"}; report {args.output}')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
