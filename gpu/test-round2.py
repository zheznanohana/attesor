#!/usr/bin/env python3
"""CPU-only production bridge unit tests. No Metal device/VT session or GUI.

Compiles both ABIs, runs native host/VT-wire tests, then runs arm64 dispatcher
and VT-wire tests through the supplied attesor WITHOUT --gpu. All outputs private.
"""
import argparse
import os
from pathlib import Path
import subprocess
import shutil
import resource

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', type=Path, default=ROOT / 'target/release/attesor')
    parser.add_argument('--sensitivity', action='store_true', help='deliberately break two private copies; require assertion failures')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    (out / 'tmp').mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(('ATTESOR_', 'MCL_', 'DYLD_'))}
    env.update(TMPDIR=str(out / 'tmp'), ATTESOR_CACHE=str(out / 'cache'), ATTESOR_NO_AOT_BUILD='1')
    if os.environ.get('ATTESOR_SYSROOT'):
        env['ATTESOR_SYSROOT'] = os.environ['ATTESOR_SYSROOT']
    frameworks = ['Foundation', 'Metal', 'VideoToolbox', 'CoreMedia', 'CoreVideo', 'IOSurface']
    def run(name, command):
        p = subprocess.run(list(map(str, command)), env=env, cwd=ROOT, capture_output=True, text=True, timeout=180)
        (out / (name + '.log')).write_text(p.stdout + p.stderr)
        if p.returncode:
            raise RuntimeError(f'{name}: exit {p.returncode}; see {out / (name + ".log")}')
        print(name + ': PASS', flush=True)
    for name, source, arch, arc in [('host', 'test-host.m', 'x86_64', True),
                                   ('vtwire-native', 'test-vtwire.m', 'x86_64', True),
                                   ('vtwire-guest', 'test-vtwire.m', 'arm64', True),
                                   ('fastpath-guest', 'test-fastpath.m', 'arm64', False)]:
        binary = out / name
        command = ['clang', '-arch', arch, '-O2', '-Wall', '-Wextra', '-Wno-unused-function',
                   '-Wno-unused-parameter', '-Wno-cast-of-sel-type',
                   '-fobjc-arc' if arc else '-fno-objc-arc']
        for framework in frameworks: command += ['-framework', framework]
        command += [ROOT / 'gpu' / source, '-o', binary]
        run('build-' + name, command)
        run(name, [binary] if arch == 'x86_64' else [args.attesor.resolve(), '--no-aot', binary])
    if args.sensitivity:
        for name, original, broken, expected in [
            ('async', 'if (async) { sync = NO; newh', 'if (async) { newh', 'cb && batches == 0'),
            ('snapshot', 'memcpy(u->bytes, p, n);', 'memset(u->bytes, 0, n);', 'bytes[k] == (uint8_t)k'),
        ]:
            directory = out / ('mutation-' + name); directory.mkdir()
            for source in (ROOT / 'gpu').iterdir():
                if source.suffix in ('.m', '.h'): shutil.copyfile(source, directory / source.name)
            source = directory / 'mclmetal.m'; text = source.read_text()
            if text.count(original) != 1: raise RuntimeError('mutation target not unique: ' + name)
            source.write_text(text.replace(original, broken))
            binary = directory / 'probe'
            cmd = ['clang', '-arch', 'arm64', '-O2', '-fno-objc-arc', '-Wno-cast-of-sel-type']
            for framework in frameworks: cmd += ['-framework', framework]
            run('build-mutation-' + name, cmd + [directory / 'test-fastpath.m', '-o', binary])
            result = subprocess.run([str(args.attesor.resolve()), '--no-aot', str(binary)], env=env,
                                    cwd=ROOT, capture_output=True, text=True, timeout=180)
            (out / ('mutation-' + name + '.log')).write_text(result.stdout + result.stderr)
            if not result.returncode or expected not in result.stderr:
                raise RuntimeError('mutation was not detected at the directed assertion: ' + name)
            print('mutation-' + name + ': detected at directed assertion', flush=True)
    return 0


if __name__ == '__main__': raise SystemExit(main())
