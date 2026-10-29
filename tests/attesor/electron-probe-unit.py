#!/usr/bin/env python3
"""Pure metadata/file tests; never starts an application, server or subprocess."""
import importlib.util
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
    print('PASS pure GPU gate, source-log isolation/rotation, owned-entry metadata, private JSON; no apps started')


if __name__ == '__main__':
    main()
