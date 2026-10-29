#!/usr/bin/env python3
"""LLDB symbol/expression/finish regression over an inherited socketpair."""
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/release/attesor').resolve())
with tempfile.TemporaryDirectory(prefix='attesor-lldb-fd-') as directory:
    directory = Path(directory)
    source = directory / 'guest.c'
    guest = directory / 'guest'
    source.write_text('''#include <stdio.h>
__attribute__((noinline)) long add3(long a, long b, long c) { return a + b * 2 + c * 3; }
int main(void) { long s = 0; for (int i = 0; i < 5; i++) s += add3(i, 10, 100); printf("sum %ld\\n", s); return 7; }
''')
    subprocess.run(['clang', '-O1', '-g', '-arch', 'arm64', source, '-o', guest], check=True)
    parent, child = socket.socketpair()
    debugger = None
    with (directory / 'error').open('w+') as error:
        process = subprocess.Popen([binary, '--gdb-fd', str(child.fileno()), str(guest)],
                                   pass_fds=[child.fileno()], stdout=subprocess.PIPE,
                                   stderr=error, text=True, start_new_session=True)
        child.close()
        try:
            deadline = time.monotonic() + 10
            slide = None
            while time.monotonic() < deadline:
                error.seek(0); text = error.read()
                match = re.search(r'--slide (0x[0-9a-f]+)', text)
                if match:
                    slide = match.group(1); break
                if process.poll() is not None: raise RuntimeError(text)
                time.sleep(0.01)
            if slide is None: raise RuntimeError('no debugger symbol hint')
            command = ['lldb', '-b', '-o', f'target create "{guest}"',
                       '-o', f'process connect --plugin gdb-remote fd://{parent.fileno()}',
                       '-o', f'target modules load --file guest --slide {slide}',
                       '-o', 'b add3', '-o', 'c', '-o', 'p a + b', '-o', 'p add3(1, 2, 3)',
                       '-o', 'register write x1 5', '-o', 'finish',
                       '-o', 'breakpoint delete', '-o', 'c']
            debugger = subprocess.Popen(command, pass_fds=[parent.fileno()],
                                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                        text=True, start_new_session=True)
            parent.close()
            try: output, _ = debugger.communicate(timeout=70)
            except subprocess.TimeoutExpired:
                os.killpg(debugger.pid, signal.SIGKILL)
                output, _ = debugger.communicate()
                raise RuntimeError('LLDB timeout\n' + output)
            print(output)
            program_output, _ = process.communicate(timeout=10)
            print(program_output)
            expected = ['add3(a=0, b=10, c=100)', '(long) 10',
                        'Return value: (long) $0 = 310', '(long) 14']
            for marker in expected:
                if marker not in output: raise AssertionError('missing LLDB result: ' + marker)
            if 'sum 1600' not in program_output: raise AssertionError('wrong program result')
            if debugger.returncode != 0 or process.returncode != 7:
                raise AssertionError(f'bad statuses lldb={debugger.returncode}, guest={process.returncode}')
            print('gdb-lldb-fd: PASS (symbols, arguments, expressions including guest function call, register write, finish, exit)')
        finally:
            parent.close()
            for running in [debugger, process]:
                if running is not None and running.poll() is None:
                    os.killpg(running.pid, signal.SIGKILL)
                    running.communicate(timeout=5)
