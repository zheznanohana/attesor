#!/usr/bin/env python3
"""Real bridge hook and legacy-library compatibility; no Metal device is opened."""
import argparse
import importlib.util
import os
from pathlib import Path
import plistlib
import subprocess

root = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('profile_report', root / 'tools/profile-report.py')
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--attesor', type=Path, default=root / 'target/release/attesor')
parser.add_argument('--bridge', type=Path, required=True)
parser.add_argument('--sysroot')
parser.add_argument('--output', type=Path, default=root / 'target/profile-bridge')
args = parser.parse_args()
args.output = args.output.resolve()
args.output.mkdir(parents=True, exist_ok=True)
tmp = args.output / 'tmp'
tmp.mkdir(exist_ok=True)
env = dict(os.environ, TMPDIR=str(tmp), ATTESOR_CACHE=str(args.output / 'aot'), ATTESOR_NO_AOT_BUILD='1')
guest = args.output / 'profile-bridge'
subprocess.run(['clang', '-arch', 'arm64', '-O2', '-pthread', str(root / 'tests/attesor/profile-bridge.c'), '-o', str(guest)], env=env, check=True)
request = args.output / 'request.plist'
request.write_bytes(plistlib.dumps(dict(c='sig', sel='description', h=0), fmt=plistlib.FMT_BINARY))
legacy_source = args.output / 'legacy.c'
legacy_source.write_text('#include <stdint.h>\nvoid *mcl_call(uint64_t op, const void *in, uint64_t len, uint64_t *n) { *n=0; return 0; }\n')
legacy = args.output / 'legacy.dylib'
subprocess.run(['clang', '-arch', 'x86_64', '-dynamiclib', str(legacy_source), '-o', str(legacy)], env=env, check=True)
command = [str(args.attesor.resolve()), '--no-aot']
if args.sysroot:
    command += ['--sysroot', args.sysroot]
for label, bridge, argument in [('hook', args.bridge.resolve(), str(request)), ('legacy', legacy, '--legacy')]:
    directory = args.output / label
    assert not directory.exists(), 'use a fresh output directory'
    run_env = dict(env, ATTESOR_GPU_BRIDGE=str(bridge), ATTESOR_PROFILE=str(directory))
    result = subprocess.run(command + [str(guest), argument], env=run_env, capture_output=True, text=True, timeout=90)
    (args.output / (label + '.log')).write_text(result.stdout + result.stderr)
    assert result.returncode == 0, (label, result.returncode, result.stdout, result.stderr)
    path, = directory.glob('*.txt')
    _, r = report.read_profile(path)
    def call(category, number, name):
        return r.calls[category, number, name]
    if label == 'hook':
        for n, name, count in [(1, 'rpc', 6), (2, 'free', 6), (4, 'batch', 6), (5, 'defsel', 1)]:
            v = call('gpu-op', n, name)
            assert v[2] == count and v[4] > 0, (n, v)
        v = call('gpu-selector', 0, 'description')
        assert v[2] == 12 and v[0] > 100 and v[4] > 100000000, v
        assert r.bridge_selectors['true'] == 1
        assert sum(v[0] for (c, _, _), v in r.calls.items() if c == 'gpu-op') == r.states['gpu']
    else:
        assert call('gpu-op', 2, 'free')[2] == 7
        assert r.bridge_selectors['false'] == 1
        assert not any(c == 'gpu-selector' for c, _, _ in r.calls)
    with (args.output / (label + '-report.txt')).open('w') as out:
        subprocess.run(['python3', str(root / 'tools/profile-report.py'), str(directory)], env=env, stdout=out, check=True)
    print(f'profile-bridge-check: PASS {label}', flush=True)
