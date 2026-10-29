#!/usr/bin/env python3
"""Isolated real Mach-O stub/runner and launcher transaction regression tests."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', help='optionally verify delegation into a real built emulator')
    parser.add_argument('--sysroot', help='ARM dyld-cache directory for the real emulator check')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    passed = 0
    sysroot = args.sysroot or os.environ.get('ATTESOR_SYSROOT')
    if args.attesor and not (sysroot and (Path(sysroot) / 'dyld_shared_cache_arm64e').is_file()):
        config = Path.home() / '.attesor/sysroot'
        if config.is_dir():
            sysroot = str(config)
        elif config.is_file():
            sysroot = config.read_text().strip()
        else:
            choices = sorted((Path.home() / 'attesor-sysroot').glob('*/dyld_shared_cache_arm64e'))
            sysroot = str(choices[-1].parent) if choices else None
        if not sysroot or not (Path(sysroot) / 'dyld_shared_cache_arm64e').is_file():
            parser.error('--attesor requires an ARM cache; provide --sysroot')

    def check(condition, name):
        nonlocal passed
        if not condition:
            raise AssertionError(name)
        passed += 1
        print('PASS', name, flush=True)

    with tempfile.TemporaryDirectory(prefix='attesor-launch-check-') as td:
        root = Path(td).resolve()
        home = root / 'home with spaces'
        launcher = home / 'attesor-launcher'
        launcher.mkdir(parents=True)
        for file in ['attesor-apps', 'stub.c', 'run-app', 'badge.swift', 'fix-dock.py']:
            shutil.copy2(repo / 'launcher' / file, launcher / file)
        (launcher / 'run-app').chmod(0o755)
        scan = root / 'Applications with spaces'
        scan.mkdir()
        state = root / 'process-state'
        state.write_text('1')
        checker = root / 'check-process'
        checker.write_text('#!/usr/bin/python3\nimport os,sys\nfrom pathlib import Path\nsys.exit(int(Path(os.environ["CHECK_STATE"]).read_text()))\n')
        checker.chmod(0o755)
        fake = root / 'fake-attesor'
        fake.write_text('#!/usr/bin/python3\nimport json,os,sys\nfrom pathlib import Path\nPath(os.environ["CAPTURE"]).write_text(json.dumps(sys.argv[1:]))\nsys.exit(int(os.environ.get("FAKE_EXIT","7")))\n')
        fake.chmod(0o755)
        capture = root / 'capture.json'
        env = dict(os.environ, HOME=str(home), ATTESOR=str(fake), ATTESOR_NO_DESKTOP='1',
                   ATTESOR_NO_ALERT='1', ATTESOR_APPS_GPU='0', CHECK_STATE=str(state),
                   CAPTURE=str(capture), ATTESOR_PROCESS_CHECK=str(checker),
                   ATTESOR_SCAN_DIRS=str(scan))

        def run(command, expect=0, extra=None):
            r = subprocess.run([str(x) for x in command], env={**env, **(extra or {})},
                               text=True, capture_output=True, timeout=90)
            if r.returncode != expect:
                logs = '\n'.join(p.read_text(errors='replace')[-8000:] for p in (home / 'Library/Logs/Attesor').glob('*.log'))
                raise RuntimeError(f'{command}: {r.returncode}, expected {expect}\n{r.stdout}\n{r.stderr}\n{logs}')
            return r

        def cli(*arguments, expect=0, extra=None):
            return run(['/usr/bin/python3', '-I', launcher / 'attesor-apps', *arguments], expect, extra)

        counter = 0
        def app(name, result=17, ident='attesor.fixture.same-id'):
            nonlocal counter
            counter += 1
            path = scan / (name + '.app')
            (path / 'Contents/MacOS').mkdir(parents=True)
            (path / 'Contents/Resources').mkdir()
            with (path / 'Contents/Info.plist').open('wb') as f:
                plistlib.dump(dict(CFBundleExecutable='Probe', CFBundleIdentifier=ident,
                                   CFBundlePackageType='APPL', CFBundleName=name), f)
            source = root / ('p' + str(counter) + '.c')
            source.write_text('#include <stdio.h>\nint main(int argc,char **argv){'
                              f'puts("guest-ok"); return {result};' + '}\n')
            exe = path / 'Contents/MacOS/Probe'
            subprocess.run(['clang', '-arch', 'arm64', '-O2', str(source), '-o', str(exe)], check=True)
            exe.chmod(0o751)
            return path, exe, exe.read_bytes()

        path, exe, original = app('Probe "quoted" $(touch SHOULD_NOT_EXIST) Ω')
        cli('inject', path)
        check(run(['lipo', '-archs', exe]).stdout.split() == ['x86_64', 'arm64'], 'universal executable contains x86 and ARM')
        thin = root / 'thin'
        run(['lipo', exe, '-thin', 'arm64', '-output', thin])
        check(thin.read_bytes() == original, 'injection preserves ARM slice byte for byte')
        check(exe.stat().st_mode & 0o777 == 0o751, 'injection preserves executable permissions')
        argv = ['', 'a b', '"quoted"', '$(touch SHOULD_NOT_EXIST)', 'Ω']
        run([exe, *argv], expect=7)
        check(json.loads(capture.read_text()) == [str(exe), *argv], 'stub/runner preserve empty, quoted, Unicode and shell-like arguments')
        check(not (exe.parent / 'SHOULD_NOT_EXIST').exists(), 'shell-like arguments are never evaluated')
        run([exe, 'gpu'], expect=7, extra={'ATTESOR_APPS_GPU': '1'})
        check(json.loads(capture.read_text()) == ['--gpu', str(exe), 'gpu'], 'ordinary app GPU flags')
        (path / 'Contents/Frameworks/Electron Framework.framework').mkdir(parents=True)
        run([exe, 'electron'], expect=7, extra={'ATTESOR_APPS_GPU': '1'})
        captured = json.loads(capture.read_text())
        check(captured[:5] == ['--gpu', '--gpu-only', '--type=gpu-process', str(exe), '--no-sandbox']
              and captured[-1] == 'electron', 'Electron flags preserve executable position and arguments')
        before = exe.read_bytes()
        state.write_text('0')
        cli('restore', path, expect=1)
        check(exe.read_bytes() == before, 'running application restore is refused')
        state.write_text('2')
        cli('restore', path, expect=1)
        check(exe.read_bytes() == before, 'unknown process visibility refuses restore')
        state.write_text('1')
        runner = launcher / 'run-app'
        runner.rename(launcher / 'run-app.hidden')
        r = run([exe], expect=127)
        check('Attesor runner' in r.stderr, 'missing runner preserves launch failure status 127')
        (launcher / 'run-app.hidden').rename(runner)
        if args.attesor:
            shutil.rmtree(path / 'Contents/Frameworks')
            run([exe], expect=17, extra={'ATTESOR': str(Path(args.attesor).resolve()),
                                       'ATTESOR_SYSROOT': sysroot})
            log = home / 'Library/Logs/Attesor' / (path.stem + '.log')
            check('guest-ok' in log.read_text(), 'real emulator executes the retained ARM slice and returns its exit status')
        failed_tools = root / 'restore-tools'
        failed_tools.mkdir()
        bad_restore = failed_tools / 'lipo'
        bad_restore.write_text('#!/bin/sh\nfor arg in "$@"; do [ "$arg" = -remove ] && exit 42; done\nexec /usr/bin/lipo "$@"\n')
        bad_restore.chmod(0o755)
        before_restore = exe.read_bytes()
        cli('restore', path, expect=1, extra={'PATH': str(failed_tools) + ':' + env.get('PATH', '')})
        check(exe.read_bytes() == before_restore, 'failed restore preparation leaves the universal executable intact')
        cli('restore', path)
        check(exe.read_bytes() == original, 'thin original restores exactly, including file layout')
        other, other_exe, other_original = app('Other same identifier', 21)
        cli('inject', path)
        cli('inject', other)
        cli('restore', path)
        cli('restore', other)
        check(exe.read_bytes() == original and other_exe.read_bytes() == other_original,
              'same bundle identifier cannot collide in backup storage')
        # A failed lipo preparation must leave the original executable in place.
        tools = root / 'tools'
        tools.mkdir()
        bad_lipo = tools / 'lipo'
        bad_lipo.write_text('#!/bin/sh\nif [ "$1" = -create ]; then exit 42; fi\nexec /usr/bin/lipo "$@"\n')
        bad_lipo.chmod(0o755)
        cli('inject', path, expect=1, extra={'PATH': str(tools) + ':' + env.get('PATH', '')})
        check(exe.read_bytes() == original, 'failed preparation leaves the original executable intact')
        # Simulate an app updater replacing only the ARM code inside a universal file.
        cli('inject', path)
        update, update_exe, update_original = app('Updater input', 33)
        replacement = root / 'replacement'
        run(['lipo', '-create', launcher / 'bin/attesor-stub', update_exe, '-output', replacement])
        replacement.chmod(0o751)
        os.replace(replacement, exe)
        cli('restore', path)
        check(exe.read_bytes() == update_original, 'restore retains updated ARM code instead of reverting backup')
        # An older owned stub is upgraded while its ARM slice stays unchanged.
        old_src = root / 'stub-old.c'
        old_src.write_text((launcher / 'stub.c').read_text().replace('ATTESOR_STUB_V2', 'ATTESOR_STUB_V1'))
        old_stub = root / 'stub-old'
        subprocess.run(['clang', '-arch', 'x86_64', '-O2', str(old_src), '-o', str(old_stub)], check=True)
        run(['lipo', '-create', old_stub, exe, '-output', replacement])
        replacement.chmod(0o751)
        os.replace(replacement, exe)
        cli('scan')
        run(['lipo', exe, '-thin', 'arm64', '-output', thin])
        check(thin.read_bytes() == update_original and b'ATTESOR_STUB_V2' in exe.read_bytes(),
              'scan upgrades an old owned stub and retains updated ARM code')
        cli('reinject')
        run(['lipo', exe, '-thin', 'arm64', '-output', thin])
        check(thin.read_bytes() == update_original, 'reinject is safe for an already-current stub')
        # Keep multiple ARM architecture slices and restore their fat layout.
        multi, multi_exe, _ = app('Multiple ARM slices')
        arm_e = root / 'arm64e'
        subprocess.run(['clang', '-arch', 'arm64e', '-O2', str(root / ('p' + str(counter) + '.c')),
                        '-o', str(arm_e)], check=True)
        run(['lipo', '-create', multi_exe, arm_e, '-output', replacement])
        replacement.chmod(0o751)
        os.replace(replacement, multi_exe)
        multi_original = multi_exe.read_bytes()
        cli('inject', multi)
        check(run(['lipo', '-archs', multi_exe]).stdout.split() == ['x86_64', 'arm64', 'arm64e'],
              'injection retains both arm64 and arm64e slices')
        cli('restore', multi)
        check(multi_exe.read_bytes() == multi_original, 'multi-ARM original fat file restores exactly')
        # A stopped thin candidate can be scanned, but running/unknown is skipped unchanged.
        pending, pending_exe, pending_original = app('Pending scan')
        state.write_text('0')
        cli('scan', expect=1)
        check(pending_exe.read_bytes() == pending_original, 'scan does not inject a running application')
        state.write_text('2')
        cli('scan', expect=1)
        check(pending_exe.read_bytes() == pending_original, 'scan does not inject when process status is unknown')
        state.write_text('1')
        # Legacy wrapper migration preserves full original resources and replaces one bundle.
        legacy, legacy_exe, legacy_original = app('Legacy wrapper')
        inner = root / 'saved-inner.app'
        legacy.rename(inner)
        (legacy / 'Contents/MacOS').mkdir(parents=True)
        (legacy / 'Contents/Resources/Original').mkdir(parents=True)
        shutil.move(str(inner), str(legacy / 'Contents/Resources/Original' / legacy.name))
        (legacy / 'Contents/MacOS/launch').write_text('#!/bin/sh\nexit 1\n')
        with (legacy / 'Contents/Info.plist').open('wb') as f:
            plistlib.dump(dict(CFBundleExecutable='launch', AttesorWrapped=True), f)
        cli('inject', legacy)
        legacy_exe = legacy / 'Contents/MacOS/Probe'
        run(['lipo', legacy_exe, '-thin', 'arm64', '-output', thin])
        check(thin.read_bytes() == legacy_original and not (legacy / 'Contents/Resources/Original').exists(),
              'legacy nested wrapper migrates to one universal bundle')
        cli('restore', legacy)
        check(legacy_exe.read_bytes() == legacy_original, 'migrated wrapper restores exact original executable')
        before_list = {p: p.read_bytes() for p in scan.glob('*.app/Contents/MacOS/*') if p.is_file()}
        listed = cli('list')
        check(str(path) in listed.stdout and all(p.read_bytes() == bits for p, bits in before_list.items()),
              'list reports handled apps without modifying application executables')
        # bundles that are no candidates (no executable name, symlinked executable, no Info.plist) are
        # skipped silently: the login agent scans every 30 s and must not log or fail on them
        (scan / 'NoExe.app/Contents/MacOS').mkdir(parents=True)
        with (scan / 'NoExe.app/Contents/Info.plist').open('wb') as f:
            plistlib.dump(dict(CFBundleName='NoExe'), f)
        (scan / 'Link.app/Contents/MacOS').mkdir(parents=True)
        with (scan / 'Link.app/Contents/Info.plist').open('wb') as f:
            plistlib.dump(dict(CFBundleExecutable='L'), f)
        (scan / 'Link.app/Contents/MacOS/L').symlink_to('/bin/ls')
        (scan / 'NoPlist.app/Contents').mkdir(parents=True)
        quiet = cli('scan')  # exit status 0 is checked by cli()
        check(quiet.stderr.strip() == '' and not any(n in quiet.stdout for n in ['NoExe', 'Link.app', 'NoPlist']),
              'scan is silent and succeeds on non-candidate bundles')
    print(f'launcher integration: {passed} checks PASS', flush=True)
    roles = ['python3', str(Path(__file__).with_name('run-app-modes.py'))]
    if args.attesor:
        roles += ['--attesor', str(Path(args.attesor).resolve())]
    if args.sysroot:
        roles += ['--sysroot', args.sysroot]
    subprocess.run(roles, check=True)


if __name__ == '__main__':
    main()
