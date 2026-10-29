#!/usr/bin/env python3
"""Real guest RSP regression over an inherited socketpair (no TCP listener needed)."""
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time

BINARY = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/release/attesor').resolve())

class RSP:
    def __init__(self, sock):
        self.sock = sock
        self.sock.settimeout(10)
        self.ack = True
    def read(self, n):
        data = b''
        while len(data) < n:
            chunk = self.sock.recv(n - len(data))
            if not chunk:
                raise RuntimeError('debugger socket closed')
            data += chunk
        return data
    def reply(self):
        assert self.read(1) == b'$'
        data = b''
        while True:
            byte = self.read(1)
            if byte == b'#': break
            data += byte
        assert int(self.read(2), 16) == sum(data) % 256
        if self.ack: self.sock.sendall(b'+')
        return data.decode('ascii')
    def send(self, data):
        data = data.encode('ascii')
        self.sock.sendall(b'$' + data + b'#' + f'{sum(data)%256:02x}'.encode())
        if self.ack: assert self.read(1) == b'+'
    def request(self, data):
        self.send(data)
        return self.reply()

with tempfile.TemporaryDirectory(prefix='attesor-gdb-protocol-') as directory:
    directory = Path(directory)
    assembly = directory / 'guest.s'
    assembly.write_text('''.text
.globl _start, _loop, _exit_test
.p2align 2
_start:
 mov x0, #0
_loop:
 add x0, x0, #1
 b _loop
_exit_test:
 mov x0, #7
 mov x16, #1
 svc #0x80
''')
    guest = directory / 'guest'
    subprocess.run(['clang', '-arch', 'arm64', '-nostdlib', '-lSystem', '-Wl,-e,_start',
                    str(assembly), '-o', str(guest)], check=True)
    symbols = {name: int(addr, 16) for addr, name in
               re.findall(r'^([0-9a-f]+) T (\S+)$', subprocess.check_output(['nm', guest], text=True), re.M)}
    results = []
    def check(name, condition):
        if not condition: raise AssertionError(name)
        results.append(name)
        print('PASS', name, flush=True)
    def session(test):
        parent, child = socket.socketpair()
        error = directory / ('error-' + str(len(results)))
        with error.open('w+') as log:
            process = subprocess.Popen([BINARY, '--gdb-fd', str(child.fileno()), str(guest)],
                                       pass_fds=[child.fileno()], stdout=subprocess.PIPE,
                                       stderr=log, start_new_session=True)
            child.close()
            try:
                rsp = RSP(parent)
                check('initial stop', rsp.request('?').startswith('T05'))
                log.flush(); log.seek(0)
                text = log.read()
                slide = int(re.search(r'--slide (0x[0-9a-f]+)', text).group(1), 16)
                test(rsp, process, slide)
            finally:
                parent.close()
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                try: process.communicate(timeout=5)
                except subprocess.TimeoutExpired: raise RuntimeError('guest did not terminate')
                log.seek(0)
                if process.returncode not in (0, 7, -signal.SIGKILL): print(log.read(), file=sys.stderr)
    def protocol(rsp, process, slide):
        check('feature negotiation', 'QStartNoAckMode+' in rsp.request('qSupported'))
        check('vCont advertises implemented actions only', rsp.request('vCont?') == 'vCont;c;s')
        check('vCont rejects unsupported action', rsp.request('vCont;t') == 'E01')
        check('malformed continue rejected', rsp.request('czz') == 'E01')
        check('register write', rsp.request('P0=8877665544332211') == 'OK')
        check('register read', rsp.request('p0') == '8877665544332211')
        check('short register write rejected', rsp.request('P0=01') == 'E01')
        check('invalid register hex rejected', rsp.request('P0=zz77665544332211') == 'E01')
        check('bad writes preserve register', rsp.request('p0') == '8877665544332211')
        before = rsp.request('g')
        check('truncated register file rejected', rsp.request('G' + before[:-2]) == 'E01')
        corrupt = '00' + before[2:-2] + 'zz'
        check('invalid register file rejected atomically', rsp.request('G' + corrupt) == 'E01' and rsp.request('g') == before)
        rsp.sock.sendall(b'$P0=0000000000000000#00')
        check('bad checksum negatively acknowledged', rsp.read(1) == b'-')
        check('bad checksum does not mutate', rsp.request('p0') == '8877665544332211')
        rsp.send('p0')
        expected = rsp.reply()
        rsp.sock.sendall(b'-')
        check('NAK retransmits last reply', rsp.reply() == expected)
        check('vector register write', rsp.request('P22=' + '0123456789abcdef' * 2) == 'OK')
        check('vector register read', rsp.request('p22') == '0123456789abcdef' * 2)
        addr = int(rsp.request('_M1000,rw'), 16)
        check('memory write', rsp.request(f'M{addr:x},4:01234567') == 'OK')
        check('memory read', rsp.request(f'm{addr:x},4') == '01234567')
        check('length mismatch rejected', rsp.request(f'M{addr:x},2:ff').startswith('E'))
        check('invalid memory hex rejected', rsp.request(f'M{addr:x},4:zz234567').startswith('E'))
        check('bad memory writes preserve bytes', rsp.request(f'm{addr:x},4') == '01234567')
        check('unknown free rejected', rsp.request(f'_m{addr+1:x}') == 'E01')
        check('debugger allocation released', rsp.request(f'_m{addr:x}') == 'OK' and rsp.request(f'm{addr:x},4') == 'E14')
        check('double free rejected', rsp.request(f'_m{addr:x}') == 'E01')
        code = int(rsp.request('_M1000,rx'), 16)
        check('upload to RX expression allocation', rsp.request(f'M{code:x},4:c0035fd6') == 'OK' and rsp.request(f'm{code:x},4') == 'c0035fd6')
        check('free RX allocation', rsp.request(f'_m{code:x}') == 'OK')
        check('huge XML offset does not overflow', rsp.request('qXfer:features:read:target.xml:ffffffffffffffff,ffffffffffffffff') == 'l')
        check('no-ack negotiation', rsp.request('QStartNoAckMode') == 'OK')
        rsp.ack = False
        check('no-ack request', rsp.request('?').startswith('T05'))
        start = symbols['_start'] + slide
        loop = symbols['_loop'] + slide
        end = symbols['_exit_test'] + slide
        check('set guest PC', rsp.request('P20=' + start.to_bytes(8, 'little').hex()) == 'OK')
        check('single step', rsp.request('s').startswith('T05') and int.from_bytes(bytes.fromhex(rsp.request('p20')), 'little') == loop)
        check('unaligned breakpoint rejected', rsp.request(f'Z0,{loop+1:x},4') == 'E01')
        check('wrong breakpoint size rejected', rsp.request(f'Z0,{loop:x},8') == 'E01')
        check('breakpoint insert', rsp.request(f'Z0,{loop:x},4') == 'OK')
        check('continue reaches breakpoint', rsp.request('c').startswith('T05'))
        check('breakpoint remove', rsp.request(f'z0,{loop:x},4') == 'OK')
        rsp.send('c'); time.sleep(0.02); rsp.sock.sendall(b'\x03')
        check('running Ctrl-C reports SIGINT', rsp.reply().startswith('T02'))
        check('stop query preserves SIGINT', rsp.request('?').startswith('T02') and rsp.request('qThreadStopInfo1').startswith('T02'))
        check('set exit PC', rsp.request('P20=' + end.to_bytes(8, 'little').hex()) == 'OK')
        check('exit status packet', rsp.request('c') == 'W07')
        process.wait(timeout=5)
        check('guest exit status', process.returncode == 7)
    session(protocol)
    def detach(rsp, process, slide):
        end = symbols['_exit_test'] + slide
        check('set detach PC', rsp.request('P20=' + end.to_bytes(8, 'little').hex()) == 'OK')
        check('detach acknowledged', rsp.request('D') == 'OK')
        process.wait(timeout=5)
        check('detached guest continues', process.returncode == 7)
    session(detach)
    def disconnect(rsp, process, slide):
        end = symbols['_exit_test'] + slide
        check('set disconnect PC', rsp.request('P20=' + end.to_bytes(8, 'little').hex()) == 'OK')
        rsp.sock.close()
        process.wait(timeout=5)
        check('disconnected guest continues', process.returncode == 7)
    session(disconnect)
    print(json.dumps(dict(passed=True, checks=len(results), binary=BINARY)))
