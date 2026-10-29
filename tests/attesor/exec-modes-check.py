#!/usr/bin/env python3
"""General exec selection/mode/stdio regression, without real applications.

Builds two differently named Electron/Chromium-shaped fixtures, thin ARM,
managed/genuine FAT and script targets. No app-name or bundle-ID special cases.
"""
import argparse
import fcntl
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile


def run(cmd, **kw):
    return subprocess.run([str(x) for x in cmd], check=True, capture_output=True, **kw)


def fat(parts, wide=False, little=False):
    endian = '<' if little else '>'
    step = 32 if wide else 20
    head = bytearray(struct.pack(endian + 'II', 0xcafebabf if wide else 0xcafebabe, len(parts)))
    payload = bytearray(); off = (8 + step * len(parts) + 16383) & ~16383
    for data in parts:
        cpu, subtype = struct.unpack_from('<II', data, 4)
        head += struct.pack(endian + ('IIQQII' if wide else 'IIIII'),
                            cpu, subtype, off, len(data), 14, *([0] if wide else []))
        payload += bytes(off - (8 + step * len(parts)) - len(payload)) + data
        off = (off + len(data) + 16383) & ~16383
    return bytes(head) + payload


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('attesor')
    p.add_argument('--filter', default='', help='run process cases whose label contains this text')
    a = p.parse_args()
    attesor = str(Path(a.attesor).resolve())
    root = Path(__file__).resolve().parents[2]
    checks = 0
    with tempfile.TemporaryDirectory(prefix='attesor-exec-modes-', dir='/tmp') as td:
        td = Path(td); host = td / 'host'; host.mkdir(); work = td / 'working'; work.mkdir()
        probe = root / 'tests/attesor/exec-mode-probe.c'
        arm, native, stub, classify = [td / name for name in ['arm-leaf', 'native-helper', 'owned-stub', 'classify']]
        run(['clang', '-O2', '-arch', 'arm64', probe, '-o', arm])
        run(['clang', '-O2', '-arch', 'x86_64', probe, '-o', native])
        run(['clang', '-O2', '-arch', 'x86_64', '-DOWNED_STUB', probe, '-o', stub])
        run(['clang', '-O2', '-arch', 'x86_64', root / 'tests/attesor/exec-arch-probe.c', '-o', classify])
        shim = host / 'libattesorexec.dylib'
        run(['clang', '-arch', 'x86_64', '-O2', '-dynamiclib', root / 'gpu/attesorexec.c', '-o', shim])
        targets = []
        for appname, framework in [('Aurora Chat', 'Electron Framework'), ('Vector Editor', 'Chromium Embedded Framework')]:
            app = td / (appname + '.app'); exe = app / 'Contents/MacOS/Main'
            exe.parent.mkdir(parents=True); (app / 'Contents/Frameworks' / (framework + '.framework')).mkdir(parents=True)
            exe.write_bytes(fat([stub.read_bytes(), arm.read_bytes()])); exe.chmod(0o755)
            targets.append(exe)
        genuine = td / 'genuine-universal'; genuine.write_bytes(fat([native.read_bytes(), arm.read_bytes()])); genuine.chmod(0o755)
        owned64 = td / 'owned-fat64'; owned64.write_bytes(fat([stub.read_bytes(), arm.read_bytes()], wide=True)); owned64.chmod(0o755)
        for wide in [False, True]:
            for little in [False, True]:
                for name, parts, want in [('managed', [stub.read_bytes(), arm.read_bytes()], '1'),
                                          ('native', [native.read_bytes(), arm.read_bytes()], '0')]:
                    file = td / f'{name}-{wide}-{little}'
                    data = fat(parts, wide, little); file.write_bytes(data)
                    assert run([classify, file], text=True).stdout.strip() == want
                    file.write_bytes(data[:-1]); assert run([classify, file], text=True).stdout.strip() == '0'
                    checks += 2
        assert run([classify, arm], text=True).stdout.strip() == '1'
        assert run([classify, native], text=True).stdout.strip() == '0'; checks += 2
        env = dict(os.environ, ATTESOR_BIN=attesor, ATTESOR_GPU_DIR=str(host),
                   ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0', ATTESOR_CACHE=str(td / 'cache'))
        args = ['--leaf', '', 'quote \' " λ', '--type=utility']
        payload = b'stdin payload\n'

        def check(label, command, expected_args, arch='arm64', mode='1', native_root=False, path=None, ipc_fd=7, copies=1, argv0=None):
            nonlocal checks
            if a.filter not in label: return
            e = dict(env, NODE_CHANNEL_FD=str(ipc_fd), TEST_IPC_FD=str(ipc_fd))
            if mode is None: e.pop('ELECTRON_RUN_AS_NODE', None)
            else: e['ELECTRON_RUN_AS_NODE'] = mode
            if argv0 is not None: e['TEST_ARGV0'] = '1'
            if copies > 1: e['TEST_SKIP_STDIN'] = '1'
            if native_root: e['DYLD_INSERT_LIBRARIES'] = str(shim)
            if path: e['PATH'] = str(path) + ':' + e.get('PATH', '')
            try: saved = os.dup(ipc_fd)
            except OSError: saved = None
            r0, w0 = os.pipe()
            rd = fcntl.fcntl(r0, fcntl.F_DUPFD_CLOEXEC, 20)
            wr = fcntl.fcntl(w0, fcntl.F_DUPFD_CLOEXEC, 20)
            os.close(r0); os.close(w0)
            try:
                os.dup2(wr, ipc_fd)
                os.close(wr)
                child = subprocess.Popen([str(x) for x in command], cwd=work, env=e,
                                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                         pass_fds=(ipc_fd,), start_new_session=True)
                try: out, err = child.communicate(payload, timeout=60)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL); out, err = child.communicate(); raise
                finally:
                    if saved is None: os.close(ipc_fd)
                    else: os.dup2(saved, ipc_fd); os.close(saved)
                ipc = os.read(rd, 128)
                def h(name, value): return name + '=' + value.encode().hex()
                expected = ['arch=' + arch] + ([h('argv0', argv0)] if argv0 is not None else []) + [h('arg', x) for x in expected_args] + [
                    h('mode', '<unset>' if mode is None else mode), h('channel', str(ipc_fd)),
                    h('cwd', str(work.resolve())), h('stdin', 'skipped\n' if copies > 1 else payload.decode())]
                expected *= copies
                assert child.returncode == 17 and out.decode().splitlines() == expected and ipc == b'IPC-preserved' * copies, \
                    (label, child.returncode, out.decode(errors='replace'), err.decode(errors='replace'), ipc, expected)
                print('PASS', label, flush=True); checks += 1
            finally: os.close(rd)

        for mode in ['1', None]:
            for method in ['spawn', 'exec']:
                for target in [arm] + targets:
                    check(f'guest {method} {target.parent.parent.parent.name} Node={mode}',
                          [attesor, '--no-aot', arm, method, target] + args, args, mode=mode)
        check('guest genuine universal stays native', [attesor, '--no-aot', arm, 'spawn', genuine] + args, args, 'x86_64')
        check('guest FAT64 managed', [attesor, '--no-aot', arm, 'spawn', owned64] + args, args)
        for target in targets:
            for method in ['spawn', 'spawnp', 'exec']:
                actual = target.name if method == 'spawnp' else target
                check(f'native {method} {target.parent.parent.parent.name}', [native, method, actual] + args,
                      args, native_root=True, path=target.parent)
            check('guest -> native -> managed ' + target.parent.parent.parent.name,
                  [attesor, '--no-aot', arm, 'spawn', native, 'exec', target] + args, args)
        check('native thin ARM', [native, 'exec', arm] + args, args, native_root=True)
        check('native genuine universal stays native', [native, 'spawn', genuine] + args, args, 'x86_64', native_root=True)
        for interpreter in [arm, owned64]:
            script = td / ('script-' + interpreter.name)
            script.write_text('#!' + str(interpreter) + ' --script-argument\nignored by probe\n'); script.chmod(0o755)
            expected = ['--script-argument', str(script)] + args
            check('native shebang ' + interpreter.name, [native, 'exec', script] + args, expected, native_root=True)
            check('guest shebang ' + interpreter.name, [attesor, '--no-aot', arm, 'spawn', script] + args, expected)
        for native_root in [False, True]:
            command = [native, 'exec', targets[0]] if native_root else [attesor, '--no-aot', arm, 'spawn', targets[1]]
            check('Node IPC FD3 ' + str(native_root), command + args, args, native_root=native_root, ipc_fd=3)
        env_script = td / 'script-env'
        env_script.write_text('#!/usr/bin/env ' + owned64.name + '\nignored\n'); env_script.chmod(0o755)
        check('native env shebang', [native, 'exec', env_script] + args, [str(env_script)] + args,
              native_root=True, path=td)
        check('guest env shebang', [attesor, '--no-aot', arm, 'spawn', env_script] + args,
              [str(env_script)] + args, path=td)
        check('eight native threads first-spawn owned ARM', [native, 'threads', targets[0]] + args,
              args, native_root=True, copies=8)
        for value in ['', '-multicall', '名字 with spaces λ']:
            for target in [arm, owned64, genuine]:
                arch = 'x86_64' if target == genuine else 'arm64'
                for method in ['spawn0', 'exec0']:
                    check('guest custom argv0 ' + repr(value), [attesor, '--no-aot', arm, method, target, value] + args,
                          args, arch=arch, argv0=value)
                    check('native custom argv0 ' + repr(value), [native, method, target, value] + args,
                          args, arch=arch, argv0=value, native_root=True)
                check('guest native guest custom argv0 ' + repr(value),
                      [attesor, '--no-aot', arm, 'spawn', native, 'exec0', target, value] + args,
                      args, arch=arch, argv0=value)
        for interpreter in [arm, owned64]:
            script = td / ('script-' + interpreter.name)
            expected = ['--script-argument', str(script)] + args
            for value in ['', 'ignored script argv0 λ']:
                for parent, extra in [([attesor, '--no-aot', arm], {}), ([native], {'native_root': True})]:
                    check('shebang replaces caller argv0 ' + repr(value),
                          parent + ['exec0', script, value] + args, expected,
                          argv0=str(interpreter), **extra)
        for optional in ['', ' --single optional argument with spaces']:
            script = td / 'script-argv-order'
            script.write_text('#!' + str(owned64) + optional + '\nignored\n'); script.chmod(0o755)
            expected = ([optional.strip()] if optional else []) + [str(script)] + args
            for parent, extra in [([attesor, '--no-aot', arm], {}), ([native], {'native_root': True})]:
                check('shebang single optional argument ' + repr(optional),
                      parent + ['spawn0', script, 'discard this argv0'] + args, expected,
                      argv0=str(owned64), **extra)
        shell = td / 'arm-shell'
        run(['lipo', '/bin/sh', '-thin', 'arm64e', '-output', shell])
        shell.chmod(0o755)
        for value in ([] if a.filter and a.filter not in 'ARM shell argv0' else ['', 'sh', '-sh', 'shell 名称 λ']):
            for parent, native_root in [([attesor, '--no-aot', arm], False), ([native], True)]:
                e = dict(env)
                if native_root: e['DYLD_INSERT_LIBRARIES'] = str(shim)
                result = run(parent + ['exec0', shell, value, '-c', 'printf "%s" "$0"'], env=e)
                assert result.stdout == value.encode(), (value, result.stdout, result.stderr)
                print('PASS ARM shell argv0', repr(value), native_root, flush=True); checks += 1
        print(f'exec-modes: PASS {checks} checks')


if __name__ == '__main__':
    main()
