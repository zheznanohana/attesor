#!/usr/bin/env python3
"""Generic stub entry roles: GUI, Electron-as-Node and Chromium child processes."""
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
    parser.add_argument('--attesor')
    parser.add_argument('--sysroot')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    count = 0

    def check(ok, message):
        nonlocal count
        if not ok:
            raise AssertionError(message)
        count += 1
        print('PASS', message, flush=True)

    with tempfile.TemporaryDirectory(prefix='attesor-launch-roles-') as td:
        root = Path(td).resolve()
        home = root / 'home with spaces'
        launcher = home / 'attesor-launcher'
        launcher.mkdir(parents=True)
        for name in ['run-app', 'stub.c', 'attesor-apps', 'badge.swift']:
            shutil.copy2(repo / 'launcher' / name, launcher / name)
        (launcher / 'run-app').chmod(0o755)
        stopped = root / 'stopped'
        stopped.write_text('#!/bin/sh\nexit 1\n')
        stopped.chmod(0o755)
        fake = root / 'fake-attesor'
        fake.write_text('''#!/usr/bin/python3
import json,os,sys,subprocess
from pathlib import Path
text=sys.stdin.read()
record=dict(pid=os.getpid(),argv=sys.argv[1:],cwd=os.getcwd(),stdin=text,
            token=os.environ.get('TEST_TOKEN'),node=os.environ.get('ELECTRON_RUN_AS_NODE'),
            node_options=os.environ.get('NODE_OPTIONS'),stderr_log=os.environ.get('ATTESOR_STDERR_LOG'))
Path(os.environ['CAPTURE']).write_text(json.dumps(record))
if os.environ.get('SPAWN_NODE') == '1':
    env=dict(os.environ,ELECTRON_RUN_AS_NODE='1',SPAWN_NODE='0')
    r=subprocess.run([os.environ['TARGET'],'worker.js','a b',''],env=env,
                     input='child-pipe',capture_output=True,text=True,cwd=os.environ['CHILD_CWD'])
    Path(os.environ['CHILD_RESULT']).write_text(json.dumps(dict(rc=r.returncode,stdout=r.stdout,stderr=r.stderr)))
print('OUT:'+text,end='')
print('ERR:'+text,end='',file=sys.stderr)
sys.exit(7)
''')
        fake.chmod(0o755)
        capture = root / 'capture.json'
        tools = root / 'tools'
        tools.mkdir()
        alert = root / 'exit-alert-called'
        osa = tools / 'osascript'
        osa.write_text('#!/bin/sh\nprintf called > "$OSASCRIPT_CALLED"\nexit 0\n')
        osa.chmod(0o755)
        env = dict(os.environ, HOME=str(home), ATTESOR=str(fake), CAPTURE=str(capture),
                   ATTESOR_PROCESS_CHECK=str(stopped), ATTESOR_NO_DESKTOP='1',
                   ATTESOR_NO_ALERT='1', ATTESOR_APPS_GPU='1', TEST_TOKEN='env " Ω',
                   NODE_OPTIONS='--preserve-caller-options', ATTESOR_STDERR_LOG='caller-value',
                   PATH=str(tools) + ':' + os.environ.get('PATH', '/usr/bin:/bin'), OSASCRIPT_CALLED=str(alert))
        env.pop('ELECTRON_RUN_AS_NODE', None)
        env.pop('SPAWN_NODE', None)

        def run(command, expect=0, extra=None, stdin=''):
            command = [str(x) for x in command]
            child = subprocess.Popen(command, env={**env, **(extra or {})}, cwd=root,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True)
            try:
                stdout, stderr = child.communicate(stdin, timeout=90)
            except subprocess.TimeoutExpired:
                child.kill()
                child.communicate()
                raise
            r = subprocess.CompletedProcess(command, child.returncode, stdout, stderr)
            r.pid = child.pid
            expected = expect if isinstance(expect, tuple) else (expect,)
            if r.returncode not in expected:
                raise RuntimeError(f'{command}: {r.returncode} != {expect}\n{r.stdout}\n{r.stderr}')
            return r

        for label, framework in [('electron-fixture', 'Electron Framework.framework'),
                                 ('chromium-fixture', 'Chromium Embedded Framework.framework')]:
            app = root / (label + ' " Ω.app')
            (app / 'Contents/MacOS').mkdir(parents=True)
            (app / 'Contents/Frameworks' / framework).mkdir(parents=True)
            with (app / 'Contents/Info.plist').open('wb') as f:
                plistlib.dump(dict(CFBundleExecutable='App', CFBundleIdentifier='local.attesor.fixture.' + label), f)
            source = root / (label + '.c')
            source.write_text('#include <stdio.h>\n#include <stdlib.h>\n#include <unistd.h>\n'
                              'int main(void){const char *p=getenv("GUEST_PID_CAPTURE");'
                              'if(p){FILE *f=fopen(p,"w");if(!f)return 90;fprintf(f,"%d\\n",getpid());fclose(f);}'
                              'puts("guest-node-ok");return 17;}\n')
            exe = app / 'Contents/MacOS/App'
            subprocess.run(['clang', '-arch', 'arm64', '-O2', str(source), '-o', str(exe)], check=True)
            run(['/usr/bin/python3', '-I', launcher / 'attesor-apps', 'inject', app])
            argv = ['', 'a b', '"quoted"', '$(touch BAD)', 'Ω']
            r = run([exe, *argv], expect=7, stdin='gui-pipe')
            data = json.loads(capture.read_text())
            log = home / 'Library/Logs/Attesor' / (app.stem + '.log')
            check(data['argv'][:5] == ['--gpu', '--gpu-only', '--type=gpu-process', str(exe), '--no-sandbox'] and
                  data['argv'][-len(argv):] == argv and r.stdout == '' and r.stderr == '' and 'OUT:gui-pipe' in log.read_text(),
                  label + ': automatic GUI policy remains enabled')
            check(data['pid'] == r.pid, label + ': GUI stub/runner/translator retain the original PID')
            before_log = log.read_bytes()
            payload = 'pipe-' + 'P' * 65536 + '\n'
            for tail, extra, mode in [
                (['worker.js', *argv], {'ELECTRON_RUN_AS_NODE': '1'}, 'Node'),
                (['--type=renderer', *argv], {}, 'renderer'),
                (['--type=utility', *argv], {}, 'utility'),
                (['--type=gpu-process', *argv], {}, 'GPU child'),
                (['--type', 'utility', *argv], {}, 'split child option'),
            ]:
                r = run([exe, *tail], expect=7, extra=extra, stdin=payload)
                data = json.loads(capture.read_text())
                check(data['argv'] == [str(exe), *tail] and data['cwd'] == str(root) and
                      data['stdin'] == payload and r.stdout == 'OUT:' + payload and r.stderr == 'ERR:' + payload and
                      data['token'] == env['TEST_TOKEN'] and data['node_options'] == env['NODE_OPTIONS'] and
                      data['stderr_log'] == env['ATTESOR_STDERR_LOG'] and log.read_bytes() == before_log,
                      label + ': ' + mode + ' argv/cwd/env/pipes/status are transparent')
                check(data['pid'] == r.pid, label + ': ' + mode + ' keeps the original PID')
            result = root / 'child-result.json'
            run([exe], expect=7, extra={'SPAWN_NODE': '1', 'TARGET': str(exe),
                                       'CHILD_CWD': str(root), 'CHILD_RESULT': str(result)})
            child = json.loads(result.read_text())
            data = json.loads(capture.read_text())
            check(child == dict(rc=7, stdout='OUT:child-pipe', stderr='ERR:child-pipe') and
                  data['argv'] == [str(exe), 'worker.js', 'a b', ''] and data['cwd'] == str(root) and data['node'] == '1',
                  label + ': GUI parent launching the same executable as Node retains IPC')
            if args.attesor:
                sysroot = args.sysroot or os.environ.get('ATTESOR_SYSROOT')
                if not sysroot:
                    choices = sorted((Path.home() / 'attesor-sysroot').glob('*/dyld_shared_cache_arm64e'))
                    sysroot = str(choices[-1].parent) if choices else None
                if not sysroot:
                    raise RuntimeError('--attesor requires --sysroot or an installed ARM dyld cache')
                pid_file = root / (label + '.pid')
                r = run([exe], expect=17, extra={'ATTESOR': str(Path(args.attesor).resolve()),
                                                'GUEST_PID_CAPTURE': str(pid_file),
                                                'ATTESOR_SYSROOT': sysroot, 'ELECTRON_RUN_AS_NODE': '1',
                                                'NODE_OPTIONS': ''})
                check(r.stdout == 'guest-node-ok\n' and r.stderr == '',
                      label + ': real ARM guest output and status cross the Node entry transparently')
                check(int(pid_file.read_text().strip()) == r.pid, label + ': real ARM Node guest retains the original PID')
                pid_file.unlink()
                r = run([exe], expect=17, extra={'ATTESOR': str(Path(args.attesor).resolve()),
                                                'ATTESOR_SYSROOT': sysroot, 'NODE_OPTIONS': '',
                                                'GUEST_PID_CAPTURE': str(pid_file), 'ATTESOR_APPS_GPU': '0'})
                check(int(pid_file.read_text().strip()) == r.pid and r.stdout == '' and r.stderr == '' and
                      'guest-node-ok' in log.read_text(), label + ': real ARM GUI guest retains PID and GUI logs')
        missing_runtime = root / 'missing-attesor-runtime'
        r = run([exe], expect=(126, 127), extra={'ATTESOR': str(missing_runtime), 'ATTESOR_NO_ALERT': '0'})
        check(str(missing_runtime) in log.read_text(), 'missing GUI runtime is logged and returns its exec failure status')
        check(not alert.exists(), 'GUI runtime failure does not start a post-exit alert supervisor')
        check(not (root / 'BAD').exists(), 'shell-like arguments are never executed')
    print(f'generic launch roles: {count} checks PASS')


if __name__ == '__main__':
    main()
