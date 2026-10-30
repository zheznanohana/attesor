#!/usr/bin/env python3
"""Build an isolated, TEST-ONLY translator fixture after a real EVEX host probe.

For sandboxes that deny sysctl: production must fail closed, but this fixture
can still exercise native translation, AOT, and benchmarks on verified hardware.
Only the copied detector is substituted; the original source is never changed.
NEVER install this binary or use its caches as production caches.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('output', type=Path)
p.add_argument('--step1-snapshot', type=Path, help='directory containing step-1 emit.rs and aot.rs')
a = p.parse_args()
repo = Path(__file__).resolve().parents[2]
out = a.output.resolve()
# Keep all generated sources, binaries and caches in the caller's worktree.
assert out.is_relative_to(repo / 'target'), 'fixture must live under this worktree/target'
out.mkdir(parents=True, exist_ok=True)
env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
env['TMPDIR'] = '/private/tmp'
probe = out / 'host-probe'
subprocess.run(['clang', '-O2', '-arch', 'x86_64', str(repo / 'tests/attesor/avx512-host-probe.c'), '-o', str(probe)], env=env, check=True)
result = subprocess.run([str(probe)], env=env, check=True, capture_output=True, text=True)
(out / 'host-probe.log').write_text(result.stdout)
print(result.stdout, end='', flush=True)
assert 'correct=1' in result.stdout
src = out / 'src'
shutil.copytree(repo / 'crates/attesor/src', src, dirs_exist_ok=True)
# The integrated native adapters are linked by the package build script. Keep
# them in the isolated fixture too; never omit host libraries from this build.
build_script = repo / 'crates/attesor/build.rs'
if build_script.exists():
    shutil.copytree(repo / 'native', out / 'native', dirs_exist_ok=True)
    (out / 'build.rs').write_text(build_script.read_text().replace('../../native/', 'native/'))
if a.step1_snapshot:
    shutil.copyfile(a.step1_snapshot / 'emit.rs', src / 'jit/emit.rs')
    shutil.copyfile(a.step1_snapshot / 'aot.rs', src / 'aot.rs')
emit = src / 'jit/emit.rs'
source = emit.read_text()
start = source.index('pub fn has_avx512() -> bool {')
end = source.index('\n#[cfg(any(target_os = "macos", test))]', start)
replacement = '''// TEST FIXTURE ONLY: a successful independent native probe authorizes this
// hardware/OS pair. No such override exists in the production translator.
pub fn has_avx512() -> bool {
    avx512_macos_policy(std::arch::x86_64::__cpuid_count(7, 0).ebx, [true; 4])
}
'''
emit.write_text(source[:start] + replacement + source[end:])
manifest = (repo / 'crates/attesor/Cargo.toml').read_text()
manifest = manifest.replace('path = "../core"', f'path = "{repo}/crates/core"')
manifest += '\n[workspace]\n\n[profile.release]\ndebug = 1\nopt-level = 3\npanic = "abort"\n'
(out / 'Cargo.toml').write_text(manifest)
shutil.copyfile(repo / 'Cargo.lock', out / 'Cargo.lock')
meta = {'warning': 'TEST ONLY; substituted OS detector, never install',
        'source_sha256': hashlib.sha256(source.encode()).hexdigest(),
        'fixture_sha256': hashlib.sha256(emit.read_bytes()).hexdigest(),
        'probe': result.stdout, 'uptime': subprocess.check_output(['uptime'], env=env, text=True).strip()}
(out / 'fixture.json').write_text(json.dumps(meta, indent=2) + '\n')
subprocess.run(['cargo', 'build', '--release', '--offline', '--manifest-path', str(out/'Cargo.toml'), '--target-dir', str(out/'build')], env=env, check=True)
print(f'TEST-ONLY binary: {out}/build/release/attesor')
