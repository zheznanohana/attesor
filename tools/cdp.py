#!/usr/bin/python3 -I
"""Drive a Chromium/Electron page over the DevTools protocol, without screen-recording permission.

  tools/cdp.py [--port 9222] [--target SUBSTR] list
  tools/cdp.py ... shot out.png          screenshot of the page contents
  tools/cdp.py ... eval 'js expression'  print the JSON result
  tools/cdp.py ... click X Y             left click at page coordinates
  tools/cdp.py ... type 'text'           insert text at the focus

The app must listen on a DevTools port (Electron/Chromium: --remote-debugging-port=N). Standard library only.
"""
import base64
import json
import os
import socket
import struct
import sys
import urllib.request


def targets(port):
    with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list', timeout=5) as r:
        return [t for t in json.load(r) if t.get('type') == 'page']


class WS:
    def __init__(self, url):
        rest = url.split('://', 1)[1]
        hostport, path = rest.split('/', 1)
        host, port = hostport.split(':')
        self.s = socket.create_connection((host, int(port)), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall((f'GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                        f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n').encode())
        resp = b''
        while b'\r\n\r\n' not in resp:
            resp += self.s.recv(4096)
        if b' 101 ' not in resp.split(b'\r\n', 1)[0]:
            raise RuntimeError(resp.split(b'\r\n', 1)[0].decode())
        self.buf = resp.split(b'\r\n\r\n', 1)[1]
        self.id = 0

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.s.recv(1 << 16)
            if not chunk:
                raise EOFError
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def send(self, obj):
        data = json.dumps(obj).encode()
        hdr = bytes([0x81])
        n = len(data)
        if n < 126:
            hdr += bytes([0x80 | n])
        elif n < 65536:
            hdr += bytes([0x80 | 126]) + struct.pack('>H', n)
        else:
            hdr += bytes([0x80 | 127]) + struct.pack('>Q', n)
        mask = os.urandom(4)
        self.s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def recv(self):
        msg = b''
        while True:
            b0, b1 = self._read(2)
            n = b1 & 0x7f
            if n == 126:
                n = struct.unpack('>H', self._read(2))[0]
            elif n == 127:
                n = struct.unpack('>Q', self._read(8))[0]
            payload = self._read(n)
            if b0 & 0x0f in (1, 2, 0):
                msg += payload
                if b0 & 0x80:
                    return json.loads(msg)

    def call(self, method, **params):
        self.id += 1
        self.send({'id': self.id, 'method': method, 'params': params})
        while True:
            m = self.recv()
            if m.get('id') == self.id:
                if 'error' in m:
                    raise RuntimeError(m['error'])
                return m.get('result', {})


def main():
    args = sys.argv[1:]
    port, want = 9222, ''
    while args and args[0].startswith('--'):
        if args[0] == '--port':
            port = int(args[1]); args = args[2:]
        elif args[0] == '--target':
            want = args[1]; args = args[2:]
        else:
            break
    if not args:
        print(__doc__); return 2
    pages = targets(port)
    if args[0] == 'list':
        for t in pages:
            print(t['title'], '|', t['url'][:120])
        return 0
    page = next((t for t in pages if want in t['title'] or want in t['url']), None)
    if page is None:
        print('no page matches', repr(want)); return 1
    ws = WS(page['webSocketDebuggerUrl'])
    cmd = args[0]
    if cmd == 'shot':
        data = ws.call('Page.captureScreenshot', format='png')['data']
        open(args[1], 'wb').write(base64.b64decode(data))
        print('wrote', args[1])
    elif cmd == 'eval':
        r = ws.call('Runtime.evaluate', expression=args[1], returnByValue=True, awaitPromise=True)
        print(json.dumps(r.get('result', {}).get('value', r), ensure_ascii=False, indent=1))
    elif cmd == 'click':
        x, y = float(args[1]), float(args[2])
        for t in ('mousePressed', 'mouseReleased'):
            ws.call('Input.dispatchMouseEvent', type=t, x=x, y=y, button='left', clickCount=1)
        print('clicked', x, y)
    elif cmd == 'type':
        ws.call('Input.insertText', text=args[1])
        print('typed', len(args[1]), 'chars')
    else:
        print(__doc__); return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
