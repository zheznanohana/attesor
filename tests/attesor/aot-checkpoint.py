#!/usr/bin/env python3
"""Verify checkpoint -> SIGKILL -> warm load, then stale-version cache repair.

Run after `cargo build --release`:
  python3 tests/attesor/aot-checkpoint.py --attesor target/release/attesor
All guest binaries, profiles and logs use a private temporary directory.
Only child processes created by this test are terminated.
"""
import argparse
import signal
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


def wait_for(process, log, pattern, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = log.read_text(errors='replace')
        if re.search(pattern, text):
            return text
        if process.poll() is not None:
            raise RuntimeError(f'guest exited {process.returncode}:\n{text}')
        time.sleep(0.05)
    raise RuntimeError(f'timeout waiting for {pattern}:\n{log.read_text(errors="replace")}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', required=True)
    args = parser.parse_args()
    attesor = str(Path(args.attesor).resolve())
    with tempfile.TemporaryDirectory(prefix='attesor-aot-checkpoint-') as td:
        root = Path(td)
        cache = root / 'cache'
        cache.mkdir()
        source = root / 'cache_wait.c'
        source.write_text('#include <stdio.h>\n#include <unistd.h>\n'
                          'int main(void) { puts("READY"); fflush(stdout); '
                          'for (;;) sleep(1); }\n')
        guest = root / 'cache_wait'
        subprocess.run(['clang', '-arch', 'arm64', '-O2', str(source), '-o', str(guest)], check=True)
        subprocess.run(['codesign', '-f', '-s', '-', str(guest)], check=True, capture_output=True)
        env = dict(os.environ, ATTESOR_CACHE=str(cache), ATTESOR_STATS='1',
                   ATTESOR_AOT_CHECKPOINT='1')
        env.pop('ATTESOR_NO_AOT_BUILD', None)
        env.pop('ATTESOR_AOT_SYNC', None)

        def run_until(label, pattern, build=True, executable=guest, extra_env=None):
            log = root / (label + '.log')
            run_env = dict(env)
            run_env.update(extra_env or {})
            if not build:
                run_env['ATTESOR_NO_AOT_BUILD'] = '1'
            with log.open('w') as out:
                process = subprocess.Popen([attesor, str(executable)], env=run_env,
                                           stdout=out, stderr=subprocess.STDOUT,
                                           start_new_session=True)
                try:
                    return wait_for(process, log, pattern)
                finally:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    process.wait(timeout=10)

        complete = r'aot checkpoint: cache_wait \(\d+ profiled blocks\): Ok'
        cold = run_until('cold', complete)
        main_files = list(cache.glob('main-*.aot'))
        assert len(main_files) == 1, list(cache.iterdir())
        path = main_files[0]
        original = path.read_bytes()
        assert len(original) >= 40 and int.from_bytes(original[16:24], 'little') > 0
        warm = run_until('warm', r'aot: loaded \d+ blocks for cache_wait', build=False)
        assert 'loaded' in warm
        print('PASS: checkpoint survived SIGKILL and next process loaded main-image blocks')
        diagnostic = run_until('diagnostic', 'READY', build=False,
                               extra_env={'ATTESOR_JIT_OFF': env.get('ATTESOR_JIT_OFF', '') + ',scalaralias'})
        assert not re.search(r'aot: loaded \d+ blocks for cache_wait', diagnostic)
        print('PASS: changed code-generation switches reject the default cache')

        # Keep the existing profile and file length/count. An incompatible file
        # must be replaced even when the profile did not grow and it is larger
        # than an in-progress builder's output.
        stale = bytearray(original)
        stale[8] ^= 0x80
        path.write_bytes(stale)
        repaired = run_until('repair', complete)
        assert not re.search(r'aot: loaded \d+ blocks for cache_wait', repaired)
        assert path.read_bytes()[8:16] == original[8:16]
        run_until('warm-repaired', r'aot: loaded \d+ blocks for cache_wait', build=False)
        print('PASS: incompatible cache rebuilt from unchanged profile and loaded on restart')
        # A builder killed after publishing its profile leaves a compatible but
        # older coverage fingerprint. That must retry even without new blocks.
        incomplete = bytearray(path.read_bytes())
        expected_hash = incomplete[32:40]
        incomplete[32] ^= 0x80
        path.write_bytes(incomplete)
        run_until('retry-coverage', complete)
        assert path.read_bytes()[32:40] == expected_hash
        print('PASS: interrupted coverage rebuild retries without new profile blocks')

        fork_source = root / 'cache_fork.c'
        fork_source.write_text(
            '#include <stdio.h>\n#include <unistd.h>\n#include <pthread.h>\n'
            'static void *worker(void *p) { (void)p; for (;;) usleep(1000); return 0; }\n'
            'int main(void) { pthread_t t[4]; '
            'for (int i=0;i<4;i++) pthread_create(&t[i],0,worker,0); '
            'usleep(100000); int child=fork(); '
            'if (child<0) return 2; '
            'if (!child) printf("CHILD %d\\n", getpid()); '
            'else printf("PARENT %d CHILD %d\\n", getpid(), child); '
            'fflush(stdout); for (;;) sleep(1); }\n')
        fork_guest = root / 'cache_fork'
        subprocess.run(['clang', '-arch', 'arm64', '-O2', str(fork_source),
                        '-o', str(fork_guest)], check=True)
        subprocess.run(['codesign', '-f', '-s', '-', str(fork_guest)], check=True, capture_output=True)
        log = root / 'fork.log'
        with log.open('w') as out:
            process = subprocess.Popen([attesor, str(fork_guest)], env=env,
                                       stdout=out, stderr=subprocess.STDOUT,
                                       start_new_session=True)
            try:
                text = wait_for(process, log, r'PARENT (\d+) CHILD (\d+)')
                parent, child = re.search(r'PARENT (\d+) CHILD (\d+)', text).groups()
                for pid in [parent, child]:
                    wait_for(process, log, r'aot checkpoint: cache_fork .*Ok.*pid=' + pid)
            finally:
                # This session contains only the emulator and guest children
                # launched by this test, never a user's running app.
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
        print('PASS: multithreaded guest fork keeps parent and child checkpoints running')
        run_until('warm-fork', r'aot: loaded \d+ blocks for cache_fork',
                  build=False, executable=fork_guest)
        print('PASS: parent/child checkpoint output survives SIGKILL and loads on restart')


if __name__ == '__main__':
    main()
