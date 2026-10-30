#!/usr/bin/env python3
"""Serial alternating phase-1/default versus opt-in loopra2; private caches, headless Node only."""
import argparse
import json
import os
import re
import subprocess
import time
from pathlib import Path

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('attesor', type=Path)
p.add_argument('--dir', type=Path, default=Path('target/loopra2/ab'))
p.add_argument('--runs', type=int, default=3)
a = p.parse_args()
a.dir.mkdir(parents=True, exist_ok=True)
root = a.dir.resolve()
env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
env.update(TMPDIR='/private/tmp', ATTESOR_CACHE=str(root/'cache'), ELECTRON_RUN_AS_NODE='1')
for name, source in [('bench', 'bench/bench.c'), ('kernels', 'bench/native-ratio/kernels.c')]:
    subprocess.run(['clang', '-O2', '-arch', 'arm64', source, '-o', str(root/name)], env=env, check=True)
rows = []
reference = None
for i in range(a.runs):
    for on in ['', 'loopra2']:
        for name, args in [('bench', [str(root/'bench')]), ('kernels', [str(root/'kernels')]),
                           ('js', ['/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness',
                                   str(Path('docs/tasks/phase2/js.js').resolve())])]:
            row = dict(run=i+1, on=on, workload=name,
                       uptime=subprocess.check_output(['uptime'], text=True).strip())
            start = time.monotonic()
            r = subprocess.run([str(a.attesor.resolve()), '--no-aot', *args], env={**env, 'ATTESOR_JIT_ON': on},
                               text=True, capture_output=True, timeout=600)
            row.update(seconds=time.monotonic()-start, returncode=r.returncode, stdout=r.stdout, stderr=r.stderr)
            rows.append(row)
            (root/'raw.json').write_text(json.dumps(rows, indent=2)+'\n')
            print(json.dumps(row), flush=True)
            if r.returncode: raise RuntimeError(f'{name} exited {r.returncode}')
            if name == 'js':
                results = dict(re.findall(r'^(\w+)\s+\d+\s+ms\s+(-?\d+)\s*$', r.stdout, re.M))
                assert len(results) == 7, results
                if reference is None: reference = results
                assert results == reference, (results, reference)
