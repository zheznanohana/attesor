#!/usr/bin/env python3
"""Serial, alternating CPU comparison; writes raw samples and medians as JSON.

Example: python3 bench/compare-cpu.py --before /tmp/attesor-before \
  --after target/release/attesor --guest bench/native-ratio/k-arm64 \
  --runs 7 --output /tmp/attesor-comparison.json
All guest runs disable AOT so different translators cannot invalidate each other's caches.
Run correctness tests separately: concurrent tests distort these measurements.
"""
import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
import time
from pathlib import Path


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--before', required=True)
    p.add_argument('--after', required=True)
    p.add_argument('--guest', required=True)
    p.add_argument('--runs', type=int, default=7)
    p.add_argument('--output', required=True)
    args = p.parse_args()
    if args.runs < 3:
        p.error('--runs must be at least 3')
    samples = []
    for round_no in range(args.runs):
        order = ['before', 'after'] if round_no % 2 == 0 else ['after', 'before']
        for label in order:
            command = [str(Path(getattr(args, label)).resolve()), '--no-aot',
                       str(Path(args.guest).resolve())]
            start = time.monotonic()
            result = subprocess.run(command, capture_output=True, text=True,
                                    check=True, timeout=300)
            metrics = {name: float(value) for name, value in
                       re.findall(r'^(\S+)\s+([\d.]+) ms$', result.stdout, re.M)}
            sample = dict(round=round_no, build=label, command=command,
                          wall_ms=(time.monotonic() - start) * 1000,
                          metrics=metrics, stdout=result.stdout, stderr=result.stderr)
            samples.append(sample)
            print(f'{round_no + 1}/{args.runs} {label}: {metrics or sample["wall_ms"]}', flush=True)
    names = samples[0]['metrics'].keys()
    if any(s['metrics'].keys() != names for s in samples):
        raise RuntimeError('benchmark output changed between runs')
    medians = {}
    for name in list(names) + ['wall_ms']:
        pair = {label: statistics.median(
            s['wall_ms'] if name == 'wall_ms' else s['metrics'][name]
            for s in samples if s['build'] == label)
            for label in ['before', 'after']}
        pair['time_reduction_percent'] = 100 * (1 - pair['after'] / pair['before'])
        medians[name] = pair
    report = dict(host=os.uname().nodename,
                  sha256={label: hashlib.sha256(Path(getattr(args, label)).read_bytes()).hexdigest()
                          for label in ['before', 'after', 'guest']},
                  samples=samples, medians=medians)
    Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(medians, indent=2))


if __name__ == '__main__':
    main()
