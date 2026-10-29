#!/usr/bin/env python3
"""Real arm64 profiler check: workload, interpreter, fork/exec, and 10s SIGKILL snapshot.

All writes go to --output (in this worktree by default). Only owned test children
are signalled. Pass --sysroot to select an installed arm64 dyld shared cache.
"""
import argparse
import importlib.util
import os
from pathlib import Path
import subprocess
import time

root = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('profile_report', root / 'tools/profile-report.py')
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--attesor', type=Path, default=root / 'target/release/attesor')
parser.add_argument('--sysroot')
parser.add_argument('--output', type=Path, default=root / 'target/profile-check')
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
args.output = args.output.resolve()
tmp = args.output / 'tmp'
tmp.mkdir(exist_ok=True)
env = dict(os.environ, TMPDIR=str(tmp), ATTESOR_CACHE=str(args.output / 'aot'),
           ATTESOR_NO_AOT_BUILD='1')
guest = args.output / 'profile-probe'
subprocess.run(['clang', '-arch', 'arm64', '-O2', '-fno-builtin',
                str(root / 'tests/attesor/profile-probe.c'), '-lz', '-pthread', '-o', str(guest)],
               env=env, check=True)
command = [str(args.attesor.resolve()), '--no-aot']
if args.sysroot:
    command += ['--sysroot', args.sysroot]


def run(label, extra=(), flags=()):
    directory = args.output / label
    assert not directory.exists(), f'use a fresh output directory: {directory}'
    run_env = dict(env, ATTESOR_PROFILE=str(directory))
    result = subprocess.run(command + list(flags) + [str(guest)] + list(extra),
                            env=run_env, text=True, capture_output=True, timeout=90)
    (args.output / (label + '.log')).write_text(result.stdout + result.stderr)
    assert result.returncode == 0, (label, result.returncode, result.stdout, result.stderr)
    files = sorted(directory.glob('*.txt'))
    assert files, label
    return [report.read_profile(path)[1] for path in files]


def calls(r, category, number):
    return [sum(v[i] for (c, n, _), v in r.calls.items() if c == category and n == number) for i in range(6)]


r, = run('workload')
assert r.states['jit'] > 0 and r.states['interp'] > 0 and r.states['idle'] > 100
assert r.blocks > 0
assert all(any(expected in name for name in r.images)
           for expected in ('profile-probe', 'libsystem_c', 'libz.'))
assert any('_main_work' in symbol for symbol in r.symbols)
subprocess.run(['python3', str(root / 'tools/profile-report.py'), str(args.output / 'workload')],
               stdout=(args.output / 'report.txt').open('w'), check=True, env=env)
print('profile-check: PASS main/libsystem_c/libz symbols, JIT/interpreter, idle sleep', flush=True)
r, = run('interp', ['--interp-probe'], ['--interp'])
assert r.states['jit'] == 0 and r.states['interp'] > 0 and r.states['idle'] > 100
print('profile-check: PASS --interp has no JIT samples', flush=True)
forked = run('fork', ['--fork'])
assert len(forked) == 2 and all(r.states['idle'] > 100 for r in forked)
assert min(sum(r.states.values()) for r in forked) < max(sum(r.states.values()) for r in forked)
# The parent's sleeping worker must not survive in the child's direct totals.
assert sorted(calls(r, 'bsd', 334)[2] for r in forked) == [1, 2]
assert all(calls(r, 'bsd', 2)[2] == 1 for r in forked)
print('profile-check: PASS multithreaded fork has independent samples/calls/times in child and parent', flush=True)
r, = run('exec', ['--exec'])
assert r.states['idle'] > 350 and r.states['jit'] > 0
assert calls(r, 'bsd', 366)[2] == 2 and calls(r, 'bsd', 59)[2] == 1
assert calls(r, 'bsd', 334)[2] == 2
print('profile-check: PASS same-pid exec preserves pre-exec samples/calls/times and inherited setting', flush=True)
r, = run('exec-fail', ['--exec-fail'])
assert r.states['idle'] > 100
assert calls(r, 'bsd', 59)[2] == 1 and calls(r, 'bsd', 59)[5] > 0
print('profile-check: PASS failed exec records the returned host interval once', flush=True)
directory = args.output / 'killed'
with (args.output / 'killed.log').open('w') as log:
    process = subprocess.Popen(command + [str(guest), '--wait'],
                               env=dict(env, ATTESOR_PROFILE=str(directory)), stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 30
        while not list(directory.glob('*.txt')):
            assert process.poll() is None, 'wait guest exited early'
            if time.monotonic() > deadline:
                raise TimeoutError('10s profile snapshot did not arrive')
            time.sleep(.05)
        path, = directory.glob('*.txt')
        r = report.read_profile(path)[1]
        assert r.states['idle'] > 1000
        # Include the unfinished tenth sleep in both total and host time.
        assert calls(r, 'bsd', 334)[5] > 8_000_000_000
        before = path.read_bytes()
        process.kill()
        process.wait(timeout=5)
        assert path.read_bytes() == before
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
print('profile-check: PASS 10s atomic snapshot includes an active host wait and survives SIGKILL', flush=True)
