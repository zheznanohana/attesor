#!/usr/bin/env python3
"""Run the unchanged phase-2 crypto/TLS helper headlessly with all flag combinations.

Crypto mismatches fail. A network error is reported as UNVERIFIED, not a TLS pass.
All writes/caches stay in this worktree. Only owned children are timed out.
"""
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/phase2-c/tls-final'
OUT.mkdir(parents=True, exist_ok=True)
base = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
for label, flags in [('off',''), ('slotreuse','slotreuse'), ('jitcost','jitcost'), ('both','jitcost,slotreuse')]:
    env = dict(base, ELECTRON_RUN_AS_NODE='1', ATTESOR_JIT_ON=flags,
               ATTESOR_CACHE=str(OUT / (label + '-cache')), ATTESOR_NO_AOT_BUILD='1',
               ATTESOR_AOT_CHECKPOINT='0', TMPDIR='/private/tmp')
    r = subprocess.run([str(ROOT / 'target/release/attesor'),
                       '/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness',
                       str(ROOT / 'docs/tasks/phase2/t.js')],
                      env=env, text=True, capture_output=True, timeout=90)
    (OUT / (label + '.log')).write_text(r.stdout + r.stderr)
    assert r.returncode == 0, (label, r.returncode, r.stderr)
    for expected in ['sha256 ca6073392ee71dbd', 'sha512 fcf47d64f5ceadd9',
                     'gcm 89788ae8c19a5009 tag de047988105bcc38', 'ecdsa verify true', 'ecdh true',
                     'string 13 e5b7a6e4b88ae8a792e9a1b6e6a08f']:
        assert expected in r.stdout, (label, r.stdout)
    if 'https status 401' in r.stdout:
        print(label, 'PASS crypto + https status 401', flush=True)
    else:
        print(label, 'PASS crypto; TLS NETWORK UNVERIFIED:', r.stdout.strip().splitlines()[-1], flush=True)
