#!/usr/bin/env python3
"""Pure metadata/file tests; never starts an application, server or subprocess."""
import importlib.util
import errno
import signal
import subprocess
import json
from pathlib import Path
import struct
import tempfile

spec = importlib.util.spec_from_file_location('electron_probe', Path(__file__).resolve().parents[2] / 'bench/electron-probe.py')
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def main():
    good = dict(auxAttributes=dict(glRenderer='ANGLE (Example GPU, Metal backend)', glVendor='Example vendor'),
                featureStatus=dict(gpu_compositing='enabled'))
    assert probe.gpu_checks(good, ['Metal', 'Example GPU'])['passed']
    for renderer in ['SwiftShader', 'llvmpipe (LLVM)', 'software renderer', 'ANGLE (lavapipe)']:
        candidate = dict(good, auxAttributes=dict(glRenderer=renderer))
        assert not probe.gpu_checks(candidate, ['.'])['passed'], renderer
    assert not probe.gpu_checks(good, ['Different vendor'])['passed']
    assert not probe.gpu_checks(dict(good, featureStatus=dict(gpu_compositing='enabled_software')), ['Metal'])['passed']
    assert not probe.gpu_checks(dict(good, featureStatus=dict(gpu_compositing='disabled_software')), ['Metal'])['passed']
    assert not probe.gpu_checks(dict(good, auxAttributes=dict(glVendor='Metal Example GPU')), ['Metal'])['passed']
    assert not probe.gpu_checks(dict(good, auxAttributes=dict(glRenderer='Metal GPU', softwareRendering=True)), ['Metal'])['passed']
    with tempfile.TemporaryDirectory(prefix='electron-probe-unit-') as td:
        root = Path(td)
        log, copy = root / 'gui.log', root / 'private.log'
        log.write_text('old ready message\n')
        tail = probe.LogTail(log, copy)
        with log.open('a') as out:
            out.write('new message\n')
        assert tail.update() == 'new message\n'
        assert copy.read_text() == 'new message\n'
        log.rename(root / 'gui.log.1')
        log.write_text('rotated new message\n')
        assert tail.update() == 'new message\nrotated new message\n'
        assert (root / 'gui.log.1').read_text() == 'old ready message\nnew message\n'
        executable = root / 'fat'
        executable.write_bytes(struct.pack('>II', 0xcafebabe, 2) +
                               struct.pack('>IIIII', 0x01000007, 3, 48, 16, 0) +
                               struct.pack('>IIIII', 0x0100000c, 0, 64, 4, 0) +
                               b'ATTESOR_STUB_V2\0\0' + b'ARM!')
        assert probe.owned_entry(executable)['owned_x86_stub']
        executable.write_bytes(b'not-owned')
        try:
            probe.owned_entry(executable)
        except ValueError:
            pass
        else:
            raise AssertionError('non-owned executable accepted')
        report = root / 'report.json'
        probe.save_report(report, {'passed': True})
        assert report.stat().st_mode & 0o777 == 0o600
        assert json.loads(report.read_text()) == {'passed': True}
    # Inject every process/signal dependency. These tests never invoke ps,
    # launch a process or deliver a real signal.
    class Child:
        pid = 100
        def wait(self, timeout):
            return 0
        def poll(self):
            return 0
    def row(pid, ppid, pgid, uid, started):
        return dict(pid=pid, ppid=ppid, pgid=pgid, uid=uid, started=started,
                    command='/test/app.app/main --secret-token=do-not-log')
    rows = {100: row(100, 1, 100, 501, 'root-start'),
            201: row(201, 100, 201, 501, 'child-one'),
            202: row(202, 100, 202, 502, 'child-two')}
    snapshots = iter([rows, rows, rows, {}])
    calls = []
    def kill_group(pgid, sig):
        calls.append(('killpg', pgid, sig))
        if sig == signal.SIGTERM:
            raise PermissionError(errno.EPERM, 'Operation not permitted')
    def kill_pid(pid, sig):
        calls.append(('kill', pid, sig))
        if pid == 201:
            raise PermissionError(errno.EPERM, 'Operation not permitted')
    detail, errors = probe.cleanup_owned(Child(), {}, Path('/test/app.app'),
        inspect=lambda: next(snapshots), name_lookup=lambda pids: {p: 'translator' for p in pids},
        send_pid=kill_pid, send_group=kill_group, pause=lambda amount: None)
    assert calls == [('killpg', 100, signal.SIGTERM), ('kill', 201, signal.SIGTERM),
                     ('kill', 202, signal.SIGTERM), ('killpg', 100, signal.SIGKILL),
                     ('kill', 201, signal.SIGKILL), ('kill', 202, signal.SIGKILL)]
    assert len(errors) == 3 and all(e['errno'] == errno.EPERM for e in errors)
    assert errors[0]['operation'] == 'killpg' and errors[0]['pgid'] == 100
    assert errors[0]['members'][0]['uid'] == 501 and errors[0]['members'][0]['command_name'] == 'translator'
    assert errors[1]['pid'] == 201 and errors[1]['process']['started'] == 'child-one'
    assert errors[1]['expected_identity']['uid'] == 501
    assert len(detail['signal_attempts']) == 6 and not detail['remaining_owned_pids']
    assert 'secret-token' not in json.dumps([detail, errors])
    # A vanished target is fully audited without treating normal self-exit as
    # failure. The final empty identity snapshot verifies cleanup completion.
    root_only = {100: rows[100]}
    vanished = iter([root_only, root_only, root_only, {}])
    def exited_group(pgid, sig):
        raise ProcessLookupError(errno.ESRCH, 'No such process')
    detail, errors = probe.cleanup_owned(Child(), {}, Path('/test/app.app'),
        inspect=lambda: next(vanished), name_lookup=lambda pids: {p: 'translator' for p in pids},
        send_pid=lambda pid, sig: (_ for _ in ()).throw(AssertionError('unexpected PID signal')),
        send_group=exited_group, pause=lambda amount: None)
    assert not errors and not detail['remaining_owned_pids']
    assert len(detail['signal_attempts']) == 2
    assert all(a['outcome'] == 'already_exited' and a['errno'] == errno.ESRCH for a in detail['signal_attempts'])
    assert detail['signal_attempts'][0]['leader']['uid'] == 501
    # A failed wait can include full Popen argv in TimeoutExpired.__str__.
    # The fallback diagnostic must not serialize that command or grant a new PID.
    class Waiting(Child):
        def poll(self):
            return None
        def wait(self, timeout):
            raise subprocess.TimeoutExpired(['/secret-program', '--api-secret=token'], timeout)
    fallback_calls = []
    def bad_snapshot():
        raise PermissionError(errno.EPERM, 'Operation not permitted')
    detail, errors = probe.cleanup_owned(Waiting(), {}, Path('/test/app.app'),
        inspect=bad_snapshot, name_lookup=lambda pids: {},
        send_pid=lambda pid, sig: (_ for _ in ()).throw(AssertionError('unexpected PID signal')),
        send_group=lambda pgid, sig: fallback_calls.append((pgid, sig)), pause=lambda amount: None)
    assert fallback_calls == [(100, signal.SIGKILL)]
    assert errors[-1]['operation'] == 'wait' and errors[-1]['timeout'] == 5
    assert 'api-secret' not in json.dumps([detail, errors]) and 'secret-program' not in json.dumps([detail, errors])
    print('PASS pure GPU/log/cache-entry/JSON checks and mocked EPERM continuation/identity/redaction and ESRCH race; no apps or signals')


if __name__ == '__main__':
    main()
