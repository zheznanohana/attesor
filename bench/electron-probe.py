#!/usr/bin/env python3
"""Read-only startup/GPU diagnostic through an owned universal app entry.

Example (run only when that application is not already running):
  python3 bench/electron-probe.py --attesor ~/bin/attesor \
    --app-executable '/Applications/Example.app/Contents/MacOS/Example' \
    --target-pattern '^https?://127\\.0\\.0\\.1:' \
    --expect-renderer Metal --expect-renderer NVIDIA --output /tmp/probe.json \
    --timeout 240 -- --some-app-argument

No DOM, page evaluation, screenshot, UI, input, or benchmark command is used.
"""
import argparse
import base64
import fcntl
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request

SCOPE = ('Owned-entry startup, matching DevTools page URL, optional application-log '
         'readiness, and browser SystemInfo GPU metadata only. Not full UI '
         'validation, frame rendering verification, or a performance benchmark.')
SOFTWARE = re.compile(r'swiftshader|llvmpipe|softpipe|lavapipe|software(?:[ _-]*(?:renderer|rasterizer|rendering))?|gdi generic|microsoft basic render', re.I)


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def save_report(path, report):
    fd, name = tempfile.mkstemp(prefix='.' + path.name + '.', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as f:
            json.dump(report, f, indent=2)
            f.write('\n')
        os.replace(name, path)  # mkstemp is private (0600), also for replacements
    finally:
        Path(name).unlink(missing_ok=True)


def owned_entry(exe):
    """Verify both ARM and an owned x86 stub without loading/executing the app."""
    size = exe.stat().st_size
    with exe.open('rb') as f:
        header = f.read(8)
        modes = {b'\xca\xfe\xba\xbe': ('>', False), b'\xbe\xba\xfe\xca': ('<', False),
                 b'\xca\xfe\xba\xbf': ('>', True), b'\xbf\xba\xfe\xca': ('<', True)}
        if len(header) != 8 or header[:4] not in modes:
            raise ValueError('app entry is not an owned universal executable; prepare it with the normal automatic launcher first')
        endian, wide = modes[header[:4]]
        count = struct.unpack(endian + 'I', header[4:])[0]
        if not 1 <= count <= 64:
            raise ValueError('invalid universal architecture count')
        records = []
        for _ in range(count):
            raw = f.read(32 if wide else 20)
            if len(raw) != (32 if wide else 20):
                raise ValueError('truncated universal header')
            if wide:
                cpu, subtype, offset, length, align, reserved = struct.unpack(endian + 'IIQQII', raw)
            else:
                cpu, subtype, offset, length, align = struct.unpack(endian + 'IIIII', raw)
            if offset > size or length > size - offset:
                raise ValueError('invalid universal slice bounds')
            records.append((cpu, offset, length))
        arm = any(cpu == 0x0100000c for cpu, _, _ in records)
        stub = False
        for cpu, offset, length in records:
            if cpu == 0x01000007 and length <= (8 << 20):
                f.seek(offset)
                stub = bool(re.search(rb'ATTESOR_STUB_V[0-9]+', f.read(length)))
                if stub:
                    break
    if not arm or not stub:
        raise ValueError('entry must contain ARM code and an Attesor-owned x86 stub')
    return dict(arm64=True, owned_x86_stub=True, sha256=digest(exe))


def processes():
    result = subprocess.run(['/bin/ps', '-axww', '-o', 'pid=,ppid=,pgid=,uid=,lstart=,command='],
                            capture_output=True, text=True, timeout=15,
                            env={**os.environ, 'LC_ALL': 'C'})
    if result.returncode:
        raise RuntimeError('cannot inspect existing/owned processes: ' + result.stderr.strip())
    rows = {}
    for line in result.stdout.splitlines():
        fields = line.split(None, 9)
        if len(fields) == 10:
            try:
                pid, ppid, pgid, uid = map(int, fields[:4])
                rows[pid] = dict(pid=pid, ppid=ppid, pgid=pgid, uid=uid,
                                 started=' '.join(fields[4:9]), command=fields[9])
            except ValueError:
                pass
    if os.getpid() not in rows:
        raise RuntimeError('process snapshot omitted the probe itself; refusing an incomplete visibility check')
    return rows


def refresh_owned(rows, owned, root_pid):
    if root_pid in rows and root_pid not in owned:
        owned[root_pid] = rows[root_pid]
    changed = True
    while changed:
        changed = False
        for pid, row in rows.items():
            parent = rows.get(row['ppid'])
            known_parent = owned.get(row['ppid'])
            linked = parent and known_parent and parent['started'] == known_parent['started']
            if pid not in owned and (row['pgid'] == root_pid or linked):
                owned[pid] = row
                changed = True
            elif pid in owned and row['started'] == owned[pid]['started']:
                owned[pid] = row  # exec changes argv, not process identity


def command_names(pids):
    """Read executable names only; never place full argv in signal errors."""
    if not pids:
        return {}
    result = subprocess.run(['/bin/ps', '-p', ','.join(str(p) for p in sorted(set(pids))),
                             '-o', 'pid=,comm='], capture_output=True, text=True, timeout=15)
    if result.returncode not in [0, 1]:
        raise RuntimeError('cannot read cleanup executable names')
    names = {}
    for line in result.stdout.splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) == 2 and fields[0].isdigit():
            names[int(fields[0])] = Path(fields[1]).name
    return names


def process_identity(row, names):
    # Explicit allowlist prevents argv, URLs, tokens and profile paths from
    # leaking into cleanup exceptions. Start time is the existing PID identity.
    if not row:
        return None
    return dict(pid=row.get('pid'), ppid=row.get('ppid'), pgid=row.get('pgid'),
                uid=row.get('uid'), started=row.get('started'),
                command_name=names.get(row.get('pid')),
                command_name_status='observed' if row.get('pid') in names else 'unavailable')


def cleanup_owned(child, owned, app, inspect=None, name_lookup=None,
                  send_pid=None, send_group=None, pause=None):
    """Same ownership boundary as the launcher; each failed signal is audited.

    Dependencies are injectable so EPERM/continuation can be verified without
    launching apps or sending a signal to any real process.
    """
    inspect = inspect or processes
    name_lookup = name_lookup or command_names
    send_pid = send_pid or os.kill
    send_group = send_group or os.killpg
    pause = pause or time.sleep
    errors, attempts = [], []
    names = {}
    last_rows = {}

    def error_record(error, operation, phase, **context):
        if isinstance(error, OSError):
            message = error.strerror or 'OS operation failed'
        elif isinstance(error, subprocess.TimeoutExpired):
            message = 'subprocess operation timed out'
        else:
            message = 'cleanup helper failed'
        return dict(operation=operation, phase=phase, error_type=type(error).__name__,
                    errno=getattr(error, 'errno', None), message=message,
                    timeout=getattr(error, 'timeout', None),
                    returncode=getattr(error, 'returncode', None),
                    caller=dict(pid=os.getpid(), uid=os.getuid(), euid=os.geteuid()), **context)

    def observe_names(rows):
        candidates = [pid for pid, row in rows.items() if row['pgid'] == child.pid or pid in owned]
        try:
            names.update(name_lookup(candidates))
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            errors.append(error_record(error, 'read_command_names', 'cleanup', pids=sorted(candidates)))

    def signal_one(operation, target, sig, rows, old=None, phase='normal'):
        if operation == 'killpg':
            members = [process_identity(row, names) for row in rows.values() if row['pgid'] == target]
            leader = rows.get(target) or owned.get(target) or dict(pid=target, pgid=target)
            context = dict(pgid=target, leader=process_identity(leader, names), members=members,
                           ownership='session created by this Popen', popen_pid=child.pid)
            send = send_group
        else:
            context = dict(pid=target, process=process_identity(rows.get(target), names),
                           expected_identity=process_identity(old, names),
                           ownership='observed descendant with matching start identity')
            send = send_pid
        action = dict(operation=operation, phase=phase, signal=int(sig),
                      signal_name=signal.Signals(sig).name, **context)
        try:
            send(target, sig)
            action['outcome'] = 'sent'
        except OSError as error:
            if error.errno == errno.ESRCH:
                # The target exited between the snapshot and signal. Retain
                # complete audit context; final identity inspection decides
                # whether cleanup completed, rather than manufacturing a FAIL.
                action.update(outcome='already_exited', errno=error.errno,
                              error_type=type(error).__name__, message=error.strerror)
            else:
                action['outcome'] = 'failed'
                errors.append(error_record(error, operation, phase, signal=int(sig),
                                           signal_name=signal.Signals(sig).name, **context))
        attempts.append(action)

    try:
        last_rows = inspect()
        refresh_owned(last_rows, owned, child.pid)
        observe_names(last_rows)
        for sig in [signal.SIGTERM, signal.SIGKILL]:
            current = inspect()
            last_rows = current
            observe_names(current)
            group = [r for r in current.values() if r['pgid'] == child.pid]
            if group:
                signal_one('killpg', child.pid, sig, current)
            for pid, old in owned.items():
                now = current.get(pid)
                if now and now['started'] == old['started'] and now['pgid'] != child.pid:
                    signal_one('kill', pid, sig, current, old=old)
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                pass
            pause(0.1)
        current = inspect()
        last_rows = current
        observe_names(current)
        remaining = [pid for pid, old in owned.items() if pid in current and
                     current[pid]['started'] == old['started']]
        if remaining:
            errors.append(dict(operation='verify_cleanup', phase='final',
                               message='owned processes still present',
                               processes=[process_identity(current[pid], names) for pid in remaining]))
        unknown = [r for r in same_app(current, app, ancestors(current)) if r['pid'] not in owned]
        if unknown:
            errors.append(dict(operation='verify_cleanup', phase='final',
                               message='unowned same-app processes remain; not signalled',
                               processes=[process_identity(row, names) for row in unknown]))
        detail = dict(owned_pids=sorted(owned), remaining_owned_pids=remaining,
                      unknown_same_app_pids=[r['pid'] for r in unknown])
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        errors.append(error_record(error, 'process_snapshot_or_wait', 'cleanup',
                                   popen_pid=child.pid,
                                   known_process=process_identity(owned.get(child.pid), names)))
        # Preserve the existing narrow fallback: only a still-live Popen handle
        # authorizes its own created session. Do not guess escaped child identity.
        if child.poll() is None:
            signal_one('killpg', child.pid, signal.SIGKILL, last_rows, phase='own-session-fallback')
            try:
                child.wait(timeout=5)
            except (OSError, subprocess.SubprocessError) as error:
                errors.append(error_record(error, 'wait', 'own-session-fallback', pid=child.pid))
        detail = dict(owned_pids=sorted(owned), remaining_owned_pids=None,
                      unknown_same_app_pids=None, verification='unavailable after inspection failure')
    detail.update(signal_attempts=attempts,
                  policy='own session plus observed descendants with matching process identity')
    return detail, errors


def listener_pids(port):
    result = subprocess.run(['/usr/sbin/lsof', '-nP', '-iTCP:' + str(port), '-sTCP:LISTEN', '-Fp'],
                            capture_output=True, text=True, timeout=15)
    if result.returncode not in [0, 1]:
        raise RuntimeError('cannot verify loopback listener ownership: ' + result.stderr.strip())
    return {int(line[1:]) for line in result.stdout.splitlines() if re.fullmatch(r'p[0-9]+', line)}


def ancestors(rows):
    skip = {os.getpid()}
    cursor = os.getppid()
    while cursor and cursor not in skip:
        skip.add(cursor)
        cursor = rows.get(cursor, {}).get('ppid', 0)
    return skip


def same_app(rows, app, skip):
    return [r for pid, r in rows.items() if pid not in skip and str(app) + '/' in r['command']]


class LogTail:
    """Copy only this launch's newly appended GUI log; never truncate the source."""
    def __init__(self, source, destination):
        self.source, self.destination = source, destination
        try:
            stat = source.stat()
            self.inode, self.offset = stat.st_ino, stat.st_size
        except FileNotFoundError:
            self.inode, self.offset = None, 0
        self.text = ''
        destination.touch(mode=0o600)

    def update(self):
        try:
            with self.source.open('rb') as f:
                stat = os.fstat(f.fileno())
                if stat.st_ino != self.inode or stat.st_size < self.offset:
                    self.offset = 0
                self.inode = stat.st_ino
                f.seek(self.offset)
                data = f.read()
                self.offset = f.tell()
            if data:
                with self.destination.open('ab') as f:
                    f.write(data)
                self.text += data.decode('utf-8', 'replace')
        except FileNotFoundError:
            pass
        return self.text


class BrowserSocket:
    """Small timeout-bounded CDP transport; its only RPC is SystemInfo.getInfo."""
    def __init__(self, url, port):
        u = urllib.parse.urlsplit(url)
        if (u.scheme != 'ws' or u.hostname not in ['127.0.0.1', 'localhost'] or
                u.port != port or u.username or u.password or u.query or u.fragment or
                not re.fullmatch(r'/devtools/browser/[A-Za-z0-9_-]+', u.path)):
            raise ValueError('browser websocket is not this probe\'s loopback endpoint')
        self.deadline = time.monotonic() + 15
        self.socket = socket.create_connection(('127.0.0.1', port), timeout=15)
        try:
            self.handshake(u.path, port)
        except Exception:
            self.socket.close()
            raise

    def handshake(self, path, port):
        self.buffer = b''
        key = base64.b64encode(os.urandom(16)).decode()
        self.socket.sendall((f'GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n'
                             'Upgrade: websocket\r\nConnection: Upgrade\r\n'
                             f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n').encode())
        response = b''
        while b'\r\n\r\n' not in response:
            if len(response) > 65536:
                raise ValueError('oversized websocket handshake')
            response += self.read_socket(4096)
        header, self.buffer = response.split(b'\r\n\r\n', 1)
        expected = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        headers = dict(line.split(b':', 1) for line in header.split(b'\r\n')[1:] if b':' in line)
        headers = {k.lower(): v.strip() for k, v in headers.items()}
        if not re.match(rb'^HTTP/1\.[01] 101\b', header) or headers.get(b'sec-websocket-accept') != expected:
            raise ValueError('invalid websocket upgrade response')

    def read_socket(self, amount):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('browser metadata request exceeded 15 seconds')
        self.socket.settimeout(min(15, remaining))
        data = self.socket.recv(amount)
        if not data:
            raise EOFError('browser websocket closed')
        return data

    def read(self, amount):
        while len(self.buffer) < amount:
            self.buffer += self.read_socket(amount - len(self.buffer))
        value, self.buffer = self.buffer[:amount], self.buffer[amount:]
        return value

    def send(self, opcode, payload):
        length = len(payload)
        head = bytes([0x80 | opcode])
        if length < 126:
            head += bytes([0x80 | length])
        elif length < 65536:
            head += bytes([0x80 | 126]) + struct.pack('>H', length)
        else:
            head += bytes([0x80 | 127]) + struct.pack('>Q', length)
        mask = os.urandom(4)
        self.socket.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def metadata(self):
        self.send(1, json.dumps(dict(id=1, method='SystemInfo.getInfo', params={})).encode())
        message = bytearray()
        while True:
            first, second = self.read(2)
            opcode, length = first & 15, second & 127
            if length == 126:
                length = struct.unpack('>H', self.read(2))[0]
            elif length == 127:
                length = struct.unpack('>Q', self.read(8))[0]
            if length > (8 << 20) or len(message) + length > (8 << 20):
                raise ValueError('oversized browser metadata message')
            mask = self.read(4) if second & 128 else None
            payload = self.read(length)
            if mask:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if opcode == 8:
                raise EOFError('browser closed metadata connection')
            if opcode == 9:
                self.send(10, payload)
                continue
            if opcode == 10:
                continue
            if opcode not in [0, 1]:
                raise ValueError('unexpected websocket data opcode')
            message.extend(payload)
            if first & 128:
                value = json.loads(message)
                message.clear()
                if value.get('id') == 1:
                    if 'error' in value:
                        raise RuntimeError('SystemInfo.getInfo failed: ' + json.dumps(value['error']))
                    return value.get('result', {}).get('gpu', {})

    def close(self):
        self.socket.close()


def gpu_checks(gpu, patterns):
    aux = gpu.get('auxAttributes', {})
    renderer = str(aux.get('glRenderer', ''))
    identity = '\n'.join(str(aux.get(k, '')) for k in
                         ['glRenderer', 'glVendor', 'glVersion', 'displayType', 'glImplementationParts'])
    statuses = gpu.get('featureStatus', {})
    composing = str(statuses.get('gpu_compositing', '')).lower()
    checks = dict(renderer_present=bool(renderer.strip()),
                  not_software=not SOFTWARE.search(identity) and str(aux.get('softwareRendering', '')).lower() not in ['true', '1'],
                  hardware_compositing=composing.startswith('enabled') and 'software' not in composing,
                  expected_renderer=[dict(pattern=p, matched=bool(re.search(p, identity, re.I))) for p in patterns])
    passed = checks['renderer_present'] and checks['not_software'] and checks['hardware_compositing'] and all(r['matched'] for r in checks['expected_renderer'])
    return dict(passed=bool(passed), renderer_identity=identity, checks=checks)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', required=True)
    parser.add_argument('--app-executable', required=True)
    parser.add_argument('--target-pattern', required=True, help='regex matched against a page target URL')
    parser.add_argument('--ready-regex', help='optional regex in this launch\'s appended application log')
    parser.add_argument('--expect-renderer', action='append', required=True,
                        help='all supplied regexes must match active renderer metadata (e.g. Metal, NVIDIA)')
    parser.add_argument('--cache-dir', help='explicit independent AOT cache to COPY into this run; never defaults to user cache')
    parser.add_argument('--output', required=True)
    parser.add_argument('--timeout', type=float, default=240)
    parser.add_argument('app_args', nargs=argparse.REMAINDER,
                        help='extra app arguments after --; literal {data} expands to a short private directory, without a shell')
    args = parser.parse_args()
    output = Path(args.output).expanduser().resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    artifact = Path(tempfile.mkdtemp(prefix=output.stem + '-', dir=output.parent))
    report = dict(success=False, scope=SCOPE, artifacts=str(artifact), output=str(output),
                  target_pattern=args.target_pattern, ready_regex=args.ready_regex,
                  expect_renderer=args.expect_renderer, cdp_methods=['SystemInfo.getInfo'],
                  elapsed_is_not_a_benchmark=True)
    child = None
    owned = {}
    lock = None
    tail = None
    browser = None
    failure = None
    startup = artifact / 'entry.log'
    startup_file = None
    data = None
    previous_term = signal.getsignal(signal.SIGTERM)
    def interrupted(signum, frame):
        raise KeyboardInterrupt('received signal ' + str(signum))
    signal.signal(signal.SIGTERM, interrupted)
    try:
        if args.timeout <= 0:
            raise ValueError('--timeout must be positive')
        target = re.compile(args.target_pattern)
        ready = re.compile(args.ready_regex) if args.ready_regex else None
        for pattern in args.expect_renderer:
            re.compile(pattern)
        exe = Path(args.app_executable).expanduser().resolve(strict=True)
        attesor = Path(args.attesor).expanduser().resolve(strict=True)
        if not os.access(attesor, os.X_OK):
            raise ValueError('translator is not executable')
        if exe.parent.name != 'MacOS' or exe.parent.parent.name != 'Contents' or exe.parent.parent.parent.suffix != '.app':
            raise ValueError('--app-executable must identify a bundle Contents/MacOS entry')
        app = exe.parent.parent.parent
        runner = Path.home() / 'attesor-launcher/run-app'
        if not runner.is_file() or not os.access(runner, os.X_OK):
            raise ValueError('installed shared runner is missing; HOME is intentionally preserved')
        report.update(app=str(app), entry=str(exe), attesor=str(attesor),
                      attesor_sha256=digest(attesor), runner=str(runner), runner_sha256=digest(runner),
                      owned_entry=owned_entry(exe), home_preserved=True)
        app_args = args.app_args[1:] if args.app_args[:1] == ['--'] else args.app_args
        report['app_argument_templates'] = app_args
        reserved = ['--user-data-dir', '--remote-debugging-port', '--remote-debugging-address']
        if any(a == k or a.startswith(k + '=') for a in app_args for k in reserved):
            raise ValueError('app arguments must not override the private profile or loopback DevTools endpoint')
        if any(a == '--type' or a.startswith('--type=') for a in app_args):
            raise ValueError('this harness starts a top-level GUI, not a Chromium --type child')
        key = hashlib.sha256(os.fsencode(app)).hexdigest()
        lockpath = Path(tempfile.gettempdir()) / ('attesor-electron-probe-' + str(os.getuid()) + '-' + key + '.lock')
        lock = os.fdopen(os.open(lockpath, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600), 'a')
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        rows = processes()
        skip = ancestors(rows)
        existing = same_app(rows, app, skip)
        if existing:
            report['existing_instance_pids'] = [r['pid'] for r in existing]
            raise RuntimeError('application is already running; no instance was started or changed')
        report['existing_instance_check'] = 'none found; fail-closed process inspection'
        cache = artifact / 'cache'
        if args.cache_dir:
            source = Path(args.cache_dir).expanduser().resolve(strict=True)
            if not source.is_dir():
                raise ValueError('--cache-dir is not a directory')
            cache.mkdir()
            copied = []
            for file in sorted(source.iterdir()):
                if file.suffix in ['.aot', '.profile'] and file.is_file():
                    if file.is_symlink():
                        raise ValueError('explicit cache contains a symlinked cache file')
                    shutil.copyfile(file, cache / file.name)
                    copied.append(dict(name=file.name, size=file.stat().st_size))
            report['copied_cache_files'] = copied
            report['cache_source'] = str(source)
            report['cache_mode'] = 'private copy of explicit source; source is never modified'
        else:
            cache.mkdir()
            report['cache_mode'] = 'new empty private cache'
        # Electron singleton sockets have short Unix path limits. Output paths
        # may be arbitrarily deep, so runtime data uses a short private /tmp root.
        data = Path(tempfile.mkdtemp(prefix='aep-', dir='/tmp')).resolve()
        profile, dsh = data / 'profile', data / 'dsh'
        profile.mkdir()
        dsh.mkdir()
        app_args = [value.replace('{data}', str(data)) for value in app_args]
        gui_log = Path.home() / 'Library/Logs/Attesor' / (app.stem + '.log')
        tail = LogTail(gui_log, artifact / 'application.log')
        report['logs'] = dict(entry=str(startup), private_application_copy=str(tail.destination),
                              existing_runner_log=str(gui_log), policy='source read-only tail from starting inode/offset; never truncated/deleted')
        with socket.socket() as reserve:
            reserve.bind(('127.0.0.1', 0))
            port = reserve.getsockname()[1]
        command = [str(exe), '--user-data-dir=' + str(profile), '--remote-debugging-address=127.0.0.1',
                   '--remote-debugging-port=' + str(port), *app_args]
        env = dict(os.environ, ATTESOR=str(attesor), ATTESOR_BIN=str(attesor), ATTESOR_CACHE=str(cache),
                   ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0', ATTESOR_STATS='1',
                   ATTESOR_NO_ALERT='1', ATTESOR_APPS_GPU='1', DSH_HOME=str(dsh))
        env.pop('ELECTRON_RUN_AS_NODE', None)
        report.update(command=command, private_profile=str(profile), private_dsh_home=str(dsh),
                      private_cache=str(cache), loopback_port=port,
                      private_data=str(data), app_arguments_use_shell=False,
                      launch_environment={k: env.get(k) for k in ['HOME', 'ATTESOR', 'ATTESOR_BIN', 'ATTESOR_CACHE',
                                          'ATTESOR_NO_AOT_BUILD', 'ATTESOR_AOT_CHECKPOINT', 'ATTESOR_STATS',
                                          'ATTESOR_NO_ALERT', 'ATTESOR_APPS_GPU', 'DSH_HOME', 'ATTESOR_GPU_DIR',
                                          'ATTESOR_JIT_ON', 'ATTESOR_JIT_OFF', 'ATTESOR_JIT_DISABLE', 'ATTESOR_NATIVE_LIBS', 'ATTESOR_NATIVE_DIR']})
        startup_file = startup.open('wb')
        child = subprocess.Popen(command, env=env, stdout=startup_file, stderr=subprocess.STDOUT, start_new_session=True)
        report['started_pid'] = child.pid
        deadline = time.monotonic() + args.timeout
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        last_error = None
        while time.monotonic() < deadline:
            rows = processes()
            refresh_owned(rows, owned, child.pid)
            text = tail.update() + startup.read_text(errors='replace')
            if child.poll() is not None:
                raise RuntimeError('owned entry exited before diagnostic readiness, status ' + str(child.returncode))
            try:
                listeners = listener_pids(port)
                if not listeners:
                    raise OSError('no owned DevTools listener yet')
                if not listeners.issubset(owned):
                    raise RuntimeError('loopback DevTools listener is not owned by this probe: ' + str(sorted(listeners)))
                report['devtools_listener_pids'] = sorted(listeners)
                with opener.open(f'http://127.0.0.1:{port}/json/version', timeout=2) as response:
                    version = json.load(response)
                with opener.open(f'http://127.0.0.1:{port}/json/list', timeout=2) as response:
                    targets = json.load(response)
                report['observed_targets'] = [{k: t.get(k) for k in ['id', 'type', 'url']} for t in targets]
                matches = [t for t in targets if t.get('type') == 'page' and target.search(t.get('url', ''))]
                if matches and (not ready or ready.search(text)):
                    listeners = listener_pids(port)
                    if not listeners:
                        raise OSError('DevTools listener disappeared before ownership verification')
                    if not listeners.issubset(owned):
                        raise RuntimeError('loopback DevTools listener is not owned by this probe: ' + str(sorted(listeners)))
                    report['devtools_listener_pids'] = sorted(listeners)
                    report['readiness'] = dict(matching_targets=[{k: t.get(k) for k in ['id', 'type', 'url']} for t in matches],
                                               log_ready=bool(ready.search(text)) if ready else None,
                                               remote_target_ready=True, scope='target URL and optional log only; full UI not assessed')
                    report['browser_version'] = {k: version.get(k) for k in ['Browser', 'Protocol-Version', 'User-Agent']}
                    browser = BrowserSocket(version['webSocketDebuggerUrl'], port)
                    gpu = browser.metadata()
                    report['gpu'] = gpu
                    report['gpu_validation'] = gpu_checks(gpu, args.expect_renderer)
                    if not report['gpu_validation']['passed']:
                        raise RuntimeError('active renderer/hardware-compositing metadata did not meet the requested GPU gate')
                    report['success'] = True
                    break
            except (OSError, ValueError, KeyError, EOFError, json.JSONDecodeError) as error:
                last_error = str(error)
                report['last_readiness_error'] = last_error
                if browser:
                    browser.close()
                    browser = None
            time.sleep(0.2)
        else:
            raise TimeoutError('diagnostic readiness timed out' + (': ' + last_error if last_error else ''))
    except (Exception, KeyboardInterrupt) as error:
        failure = f'{type(error).__name__}: {error}'
        report['error'] = failure
        report['success'] = False
    finally:
        if browser:
            try:
                browser.close()
            except OSError as error:
                report['socket_close_error'] = str(error)
                report['success'] = False
        if child:
            detail, cleanup_errors = cleanup_owned(child, owned, app)
            report['cleanup'] = detail
            if cleanup_errors:
                report['cleanup_errors'] = cleanup_errors
                report.setdefault('error', 'owned-process cleanup failed; see cleanup_errors')
                report['success'] = False
        if tail:
            try:
                tail.update()
                report['aot_load_evidence'] = re.findall(r'^.*aot: loaded .*$', tail.text, re.M)
            except OSError as error:
                report['log_copy_error'] = str(error)
                report['success'] = False
        if startup_file:
            startup_file.close()
        if data:
            # Retain failed-run data for diagnosis; delete only private data on
            # a successful, clean run. Explicit source caches are never removed.
            if report['success']:
                try:
                    shutil.rmtree(data)
                except OSError as error:
                    report['private_data_cleanup_error'] = str(error)
                    report['success'] = False
            report['private_data_removed'] = not data.exists()
        if lock:
            lock.close()
        save_report(output, report)
        signal.signal(signal.SIGTERM, previous_term)
    print(('PASS' if report['success'] else 'FAIL') + ': ' + str(output))
    if failure:
        print(failure, file=sys.stderr)
    return 0 if report['success'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
