#!/usr/bin/env python3
"""Build/run the arm64 syscall workload with all writes isolated under --output."""
import argparse
import importlib.util
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('profile_report', root / 'tools/profile-report.py')
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--attesor', type=Path, default=root / 'target/release/attesor')
parser.add_argument('--sysroot')
parser.add_argument('--output', type=Path, default=root / 'target/profile-syscalls')
args = parser.parse_args()
args.output = args.output.resolve()
args.output.mkdir(parents=True, exist_ok=True)
tmp = args.output / 'tmp'
tmp.mkdir(exist_ok=True)
profile = args.output / 'profile'
assert not profile.exists(), 'use a fresh output directory'
env = dict(os.environ, TMPDIR=str(tmp), ATTESOR_PROFILE=str(profile),
           ATTESOR_CACHE=str(args.output / 'aot'), ATTESOR_NO_AOT_BUILD='1')
guest = args.output / 'profile-syscalls'
fixture = args.output / 'fixture'
fixture.write_bytes(b'x' * 64)
subprocess.run(['clang', '-arch', 'arm64', '-O2', '-pthread',
                str(root / 'tests/attesor/profile-syscalls.c'), '-o', str(guest)], env=env, check=True)
command = [str(args.attesor.resolve()), '--no-aot']
if args.sysroot:
    command += ['--sysroot', args.sysroot]
result = subprocess.run(command + [str(guest), str(fixture)], env=env,
                        text=True, capture_output=True, timeout=120)
(args.output / 'guest.log').write_text(result.stdout + result.stderr)
assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
assert 'PASS iterations=20000' in result.stdout
path, = profile.glob('*.txt')
_, r = report.read_profile(path)
# Startup may issue additional calls; accept libc's cancel/nocancel variants.
def counts(category, numbers):
    return [sum(v[i] for (c, n, _), v in r.calls.items() if c == category and n in numbers) for i in range(6)]
for numbers in ({338, 188}, {5, 398, 463, 464}, {3, 396}, {6, 399}, {20}, {116}, {363, 369}):
    v = counts('bsd', numbers)
    assert v[2] >= 20000, (numbers, v)
    assert v[4] > v[5] > 0, (numbers, v)
v = counts('mach', {3})
assert v[2] >= 20000 and v[4] > 0 and v[5] == 0, v
v = counts('bsd', {363, 369})
assert v[3] >= 20000, v
assert r.states['idle'] > 100
assert any(c == 'bsd' and n in (334, 423) and v[1] > 100 for (c, n, _), v in r.calls.items())
assert sum(v[0] for (c, _, _), v in r.calls.items() if c in ('bsd', 'mach', 'machdep')) == sum(r.states[s] for s in ('syscall-bsd', 'syscall-mach', 'syscall-machdep', 'idle'))
with (args.output / 'report.txt').open('w') as out:
    subprocess.run(['python3', str(root / 'tools/profile-report.py'), str(profile)], stdout=out, env=env, check=True)
print(result.stdout.strip())
print('profile-syscalls-check: PASS syscall counts, indirect getpid, kernel/emulation split, idle sleep and zero-timeout kevent')
print('report:', args.output / 'report.txt')

off_env = {key: value for key, value in env.items() if key != 'ATTESOR_PROFILE'}
off = subprocess.run(command + [str(guest), str(fixture)], env=off_env,
                     text=True, capture_output=True, timeout=120)
(args.output / 'disabled.log').write_text(off.stdout + off.stderr)
assert off.returncode == 0 and 'PASS iterations=20000' in off.stdout
assert list(profile.glob('*.txt')) == [path]
print('profile-syscalls-check: PASS same workload with profiler disabled')
