#!/usr/bin/env python3
"""Real ARM startup policy tests, with a private HOME and .app fixture."""
import argparse
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--attesor', default='target/release/attesor')
p.add_argument('--sysroot', required=True)
args = p.parse_args()
binary = Path(args.attesor).resolve()
with tempfile.TemporaryDirectory(prefix='attesor-policy-') as d:
    d = Path(d)
    app = d / 'Fixture.app/Contents'
    (app / 'MacOS').mkdir(parents=True)
    (app / 'Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier': 'com.example.Policy'}, fmt=plistlib.FMT_BINARY))
    source = d / 'probe.c'
    source.write_text('#include <stdio.h>\nint main(void) { puts("policy-fixture-ok"); return 0; }\n')
    exe = app / 'MacOS/probe'
    subprocess.run(['clang', '-arch', 'arm64', '-O2', str(source), '-o', str(exe)], check=True)
    policy = d / 'Library/Application Support/Attesor/policy.json'
    policy.parent.mkdir(parents=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith(('ATTESOR_', 'DYLD_', 'MCL_'))}
    env.update(HOME=str(d), ATTESOR_CACHE=str(d / 'cache'))
    for label, content, switch, warning in [
        ('missing', None, '1', False),
        ('empty', '{}', '1', False),
        ('empty-allowlist', '{"com.example.Policy":["callret","regioncache"]}', '1', False),
        ('other-app', '{"com.example.Other":["callret"]}', '1', False),
        ('malformed', '{', '1', True),
        ('unknown', '{"com.example.Policy":["not-reviewed"]}', '1', True),
        ('duplicate', '{"com.example.Policy":[],"com.example.Policy":[]}', '1', True),
        ('oversize', ' ' * 65537, '1', True),
        ('kill-switch', '{', '0', False),
    ]:
        if content is not None:
            policy.write_text(content)
        r = subprocess.run([str(binary), '--no-aot', '--sysroot', args.sysroot, str(exe)],
                           env={**env, 'ATTESOR_AUTOTUNE': switch}, text=True,
                           capture_output=True, timeout=60)
        assert r.returncode == 0 and r.stdout.strip() == 'policy-fixture-ok', (label, r.returncode, r.stdout, r.stderr)
        assert ('attesor: policy ignored:' in r.stderr) == warning, (label, r.stderr)
        print('PASS ' + label)
print('policy-check: 9 startup cases passed; private HOME only')
