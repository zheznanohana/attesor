#!/usr/bin/env python3
"""Verify static prewarm without executing constructors/main, then actual loads.

Every binary, .app, cache, sentinel and profile belongs to a private temp dir.
No installed app or user application data is read or modified by these tests.
"""
import argparse
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import tempfile




def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', required=True)
    parser.add_argument('--output')
    args = parser.parse_args()
    binary = str(Path(args.attesor).resolve())
    checks = []

    def check(label, condition):
        if not condition:
            raise AssertionError(label)
        checks.append(label)
        print('PASS:', label, flush=True)

    with tempfile.TemporaryDirectory(prefix='attesor-prewarm-check-') as td:
        root = Path(td)
        source = root / 'probe.c'
        source.write_text(Path(__file__).with_name('aot-prewarm-probe.c').read_text())
        sentinel = root / 'guest-ran'
        env = dict(os.environ, PREWARM_SENTINEL=str(sentinel), ATTESOR_STATS='1',
                   ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0')
        for key in ['ATTESOR_JIT_ON', 'ATTESOR_JIT_OFF', 'ATTESOR_JIT_DISABLE',
                    'ATTESOR_NO_COMMPAGE', 'ATTESOR_NATIVE_LIBS', 'ATTESOR_GPU']:
            env.pop(key, None)

        def compile_to(exe, arch='arm64', stripped=False):
            exe.parent.mkdir(parents=True, exist_ok=True)
            command = ['clang', '-arch', arch, '-O2', str(source), '-o', str(exe)]
            if stripped:
                command += ['-Wl,-no_function_starts']
            subprocess.run(command, check=True, capture_output=True)

        def run(argv, cache, extra=None, success=True):
            r = subprocess.run([binary] + [str(a) for a in argv],
                               env={**env, 'ATTESOR_CACHE': str(cache), **(extra or {})},
                               text=True, capture_output=True, timeout=180)
            if success and r.returncode:
                raise RuntimeError(f'{argv}: {r.returncode}\n{r.stdout}\n{r.stderr}')
            if not success and not r.returncode:
                raise AssertionError(f'expected failure: {argv}')
            return r

        def prewarm(argv, cache, extra=None):
            sentinel.unlink(missing_ok=True)
            r = run(argv, cache, extra)
            check('prewarm executes neither constructor nor main',
                  not sentinel.exists() and not r.stdout and 'guest code was not executed' in r.stderr)
            return r

        def runtime(exe, cache, extra=None):
            sentinel.unlink(missing_ok=True)
            r = run([exe], cache, extra)
            check('actual execution reaches constructor and checksum',
                  sentinel.exists() and r.stdout.startswith('prewarm-checksum '))
            check('new process actually loads main and dyld translations',
                  re.search(r'aot: loaded [1-9]\d* blocks for ' + re.escape(exe.name) + r'\n', r.stderr)
                  and re.search(r'aot: loaded [1-9]\d* blocks for dyld\n', r.stderr))
            return r

        guest = root / 'direct'
        native = root / 'native'
        compile_to(guest)
        compile_to(native, 'x86_64')
        reference = subprocess.run([native], env=env, text=True, capture_output=True, check=True).stdout
        cache = root / 'cache'
        r = prewarm(['--prewarm', guest], cache)
        check('main and dyld are statically translated', 'prewarm complete: 2 images,' in r.stderr)
        check('prewarm does not create an execution profile', not list(cache.glob('*.profile')))
        main_file, = cache.glob('main-*.aot')
        data = main_file.read_bytes()
        profile = main_file.with_suffix('.profile')
        profile.write_bytes(data[40:48])
        before = profile.read_bytes()
        prewarm(['--aot', '--dyld', '/usr/lib/dyld', guest], cache)
        check('explicit prewarm preserves an existing execution profile', profile.read_bytes() == before)
        check('prewarmed runtime matches independent native output', runtime(guest, cache).stdout == reference)

        redirected = root / 'redirected-cache'
        prewarm(['--prewarm', guest], redirected, {'ATTESOR_NO_COMMPAGE': '1'})
        check('redirected commpage compatibility is recorded',
              struct.unpack_from('<I', next(redirected.glob('main-*.aot')).read_bytes(), 12)[0] & 1)
        check('redirected prewarm is reusable by a new process',
              runtime(guest, redirected, {'ATTESOR_NO_COMMPAGE': '1'}).stdout == reference)

        stripped = root / 'no-function-starts'
        compile_to(stripped, stripped=True)
        stripped_cache = root / 'stripped-cache'
        prewarm(['--prewarm', stripped], stripped_cache)
        symbols = subprocess.run(['nm', '-n', str(stripped)], capture_output=True, text=True, check=True).stdout
        leaf = int(re.search(r'^([0-9a-f]+) . _leaf$', symbols, re.M).group(1), 16)
        header = int(re.search(r'^([0-9a-f]+) . __mh_execute_header$', symbols, re.M).group(1), 16)
        data = next(stripped_cache.glob('main-*.aot')).read_bytes()
        count = struct.unpack_from('<Q', data, 16)[0]
        pcs = {struct.unpack_from('<Q', data, 40 + 16 * i)[0] for i in range(count)}
        check('entry fallback follows BL to its callee without function starts',
              0x500000000 + leaf - header in pcs)
        check('stripped executable runs from prewarmed translations', runtime(stripped, stripped_cache).stdout == reference)

        app = root / 'Space 引号.app'
        main_exe = app / 'Contents/MacOS/Main'
        helper = app / 'Contents/Frameworks/Helper.app/Contents/MacOS/Helper'
        compile_to(main_exe)
        compile_to(helper)
        compile_to(app / 'Contents/MacOS/NativeOnly', 'x86_64')
        resources = app / 'Contents/Resources'
        resources.mkdir()
        (resources / 'text').write_text('not code')
        (resources / 'cycle').symlink_to(app / 'Contents', target_is_directory=True)
        alias = root / 'main-alias'
        alias.symlink_to(main_exe)
        bundle_cache = root / 'bundle-cache'
        r = prewarm(['--prewarm', app, alias, app], bundle_cache)
        check('bundle/helper discovery deduplicates aliases and skips native/resources/symlink loops',
              'prewarm complete: 3 images,' in r.stderr and len(list(bundle_cache.glob('main-*.aot'))) == 2)
        for exe in [main_exe, helper]:
            check('bundle executable has a reusable runtime cache', runtime(exe, bundle_cache).stdout == reference)

        for mode in ['--interp', '--no-aot']:
            bad_cache = root / ('bad-' + mode.lstrip('-'))
            r = run(['--prewarm', mode, guest], bad_cache, success=False)
            check('incompatible ' + mode + ' is rejected before cache writes',
                  'cannot be combined' in r.stderr and not bad_cache.exists())
        r = run(['--prewarm', native], root / 'bad-native', success=False)
        check('native-only targets are rejected clearly', 'not an arm64 executable' in r.stderr)

    report = dict(passed=True, checks=checks, binary=binary)
    if args.output:
        Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
    print(f'aot-prewarm: PASS ({len(checks)} checks)')


if __name__ == '__main__':
    main()
