#!/usr/bin/env python3
"""Track-P QoS opt-in checks; private caches, no GUI and no translator changes."""
import argparse
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]


def check(binary, root):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith('ATTESOR_') and k != 'DYLD_INSERT_LIBRARIES'}
    env.update(TMPDIR='/private/tmp', ATTESOR_AOT_CHECKPOINT='0', ATTESOR_STATS='1')
    source, guest, native = root/'probe.c', root/'probe-arm', root/'probe-x86'
    source.write_text('#include <stdio.h>\n#include <unistd.h>\n#include <errno.h>\n'
                      'int main(int argc, char **argv) { if(argc>1) {'
                      'char *a[]={"missing",0}; execv("/attesor-process-qos-missing",a);'
                      'if(errno!=ENOENT)return 2;} puts("checksum 1999000"); return 0;}\n')
    for arch, dest in [('arm64', guest), ('x86_64', native)]:
        subprocess.run(['clang', '-O2', '-arch', arch, str(source), '-o', str(dest)], env=env, check=True)
    expected = subprocess.check_output([str(native), 'exec-fail'], env=env, text=True)

    def run(label, args, extra):
        run_env = {**env, 'ATTESOR_CACHE': str(root/label), **extra}
        p = subprocess.run([str(binary), *map(str, args)], env=run_env, text=True,
                           capture_output=True, check=True, timeout=180)
        (root/(label+'.stdout')).write_text(p.stdout)
        (root/(label+'.stderr')).write_text(p.stderr)
        return p

    # User-awaited --prewarm must retain its inherited policy. ON is explicit;
    # OFF wins. Compare actual generated payloads, not only cache existence.
    payloads = []
    for label, flags in [
        ('default', {}),
        ('on', {'ATTESOR_JIT_ON': 'process-qos'}),
        ('off-wins', {'ATTESOR_JIT_ON': 'process-qos', 'ATTESOR_JIT_OFF': 'process-qos'}),
    ]:
        p = run(label, ['--prewarm', guest], flags)
        assert 'process-qos:' not in p.stderr, 'waited prewarm must not call QoS policy APIs'
        files = sorted((root/label).glob('*.aot'))
        assert len(files) == 2, files
        blobs = []
        for f in files:
            data = f.read_bytes()
            n = int.from_bytes(data[16:24], 'little')
            start = (40 + n*16 + 0x3fff) & ~0x3fff
            # Counts, index and code all match; only compatibility id differs.
            blobs.append((f.name, data[16:32], data[40:40+n*16], data[start:]))
        payloads.append(blobs)
    assert payloads[0] == payloads[1] == payloads[2], 'QoS must not change generated code'
    print('PASS prewarm: explicit ON, OFF precedence, inherited QoS, no Darwin BG, identical AOT indexes/code', flush=True)

    for label, extra in [('interp', {}), ('sync-default', {}),
                         ('sync-on', {'ATTESOR_JIT_ON': 'process-qos'})]:
        args = ['--interp', guest, 'exec-fail'] if label == 'interp' else [guest, 'exec-fail']
        p = run(label, args, {**extra, 'ATTESOR_AOT_SYNC': '1'})
        assert p.stdout == expected, (label, p.stdout, expected)
        assert 'process-qos:' not in p.stderr, 'sync saves must not lower a guest thread'
    print('PASS native=interpreter=JIT/sync AOT output; failed exec returns; sync guest QoS untouched', flush=True)

    # Exactly one exit builder; its pid is published before it detaches. Wait
    # for it rather than leaving translation running after the test finishes.
    p = run('async-on', [guest], {'ATTESOR_JIT_ON': 'process-qos'})
    workers = re.findall(r'process-qos: role=aot-exit pid=(\d+) qos_result=(\d+) qos=(0x[0-9a-f]+) darwin_result=(-?\d+) darwin_bg=(-?\d+)', p.stderr)
    assert len(workers) == 1, p.stderr
    pid, qos_result, qos, bg_result, bg = workers[0]
    pid = int(pid)
    running = True
    try:
        deadline = time.monotonic() + 180
        while True:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                running = False
                break
            if time.monotonic() >= deadline:
                raise TimeoutError(f'own AOT worker {pid} did not exit')
            time.sleep(0.05)
    finally:
        # No process searches or broad kills: only the builder from this run.
        if running:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    assert list((root/'async-on').glob('main-*.aot')), 'builder must actually publish translations'
    assert p.stdout == expected, p.stdout
    assert bg_result == '0' and bg == '0', p.stderr
    assert (qos_result == '0' and qos == '0x15') or qos_result == '1', p.stderr
    print(f'PASS detached exit builder: qos_result={qos_result} qos={qos} darwin_result={bg_result} darwin_bg={bg}; published AOT and exited', flush=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', type=Path, default=REPO/'target/release/attesor')
    args = p.parse_args()
    with tempfile.TemporaryDirectory(prefix='attesor-process-qos-', dir='/private/tmp') as td:
        check(args.attesor.resolve(), Path(td))


if __name__ == '__main__':
    main()
