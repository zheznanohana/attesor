#!/usr/bin/env python3
"""Track C: alternating cold/private A/B, raw JS results, uptime and profiles.

Ordinary timing and intrusive jitcost phase probes are kept in separate series.
No GUI or shared Attesor cache is used. Times include Electron/Node startup.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--attesor', type=Path, default=ROOT / 'target/release/attesor')
p.add_argument('--electron', type=Path, default=Path('/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness'))
p.add_argument('--output', type=Path, required=True)
p.add_argument('--rounds', type=int, default=3)
p.add_argument('--after', default='slotreuse')
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=False)
a.output = a.output.resolve()
spec = importlib.util.spec_from_file_location('profile_report', ROOT / 'tools/profile-report.py')
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
expected = dict(loops=28665, objects=-259384288, json=80000, regex=400000,
                strings=2025151008, closures=19360, sort=6448448)
rows = []
for series in ['plain', 'probe']:
    for k in range(a.rounds):
        for variant in ['before', 'after']:
            label = f'{series}-{k + 1}-{variant}'
            flags = (['jitcost'] if series == 'probe' else []) + (a.after.split(',') if variant == 'after' else [])
            directory = a.output / label
            directory.mkdir()
            env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
            env.update(ELECTRON_RUN_AS_NODE='1', ATTESOR_CACHE=str(directory / 'cache'),
                       ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0', ATTESOR_JIT_ON=','.join(flags))
            if series == 'probe': env['ATTESOR_PROFILE'] = str(directory / 'profile')
            uptime = subprocess.check_output(['uptime'], text=True).strip()
            command = ['/usr/bin/time', '-p', str(a.attesor.resolve()), str(a.electron), str(ROOT / 'docs/tasks/phase2/js.js')]
            start = time.monotonic()
            r = subprocess.run(command, env=env, text=True, capture_output=True, timeout=180)
            elapsed = time.monotonic() - start
            (directory / 'raw.log').write_text(f'uptime: {uptime}\nflags: {flags}\n' + r.stdout + r.stderr)
            assert r.returncode == 0, (label, r.returncode, r.stderr)
            results = {name: (int(ms), int(value)) for name, ms, value in
                       re.findall(r'^(loops|objects|json|regex|strings|closures|sort)\s+(\d+) ms (-?\d+)$', r.stdout, re.M)}
            assert {name: value for name, (_, value) in results.items()} == expected, (label, results)
            row = dict(label=label, series=series, variant=variant, round=k+1, flags=flags,
                       uptime=uptime, elapsed=elapsed, ms={name: ms for name, (ms, _) in results.items()})
            if series == 'probe':
                files = list((directory / 'profile').glob('*.txt'))
                assert len(files) == 1, files
                _, profile = report.read_profile(files[0])
                row.update(states=dict(profile.states), metrics=dict(profile.jit_metrics),
                           blocks=profile.blocks, translation_ms=profile.ms)
                summary = subprocess.check_output(['python3', str(ROOT / 'tools/profile-report.py'), str(directory / 'profile')], text=True)
                (directory / 'summary.txt').write_text(summary)
            rows.append(row)
            (a.output / 'results.json').write_text(json.dumps(rows, indent=2) + '\n')
            print(label, f'{elapsed:.3f}s', row['ms'], flush=True)
print('PASS: every JS checksum matched in all alternating runs')
