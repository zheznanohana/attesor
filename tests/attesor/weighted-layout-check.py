#!/usr/bin/env python3
"""Private whole-file weighted AOT placement, reload, OFF and content identity."""
import os
from pathlib import Path
import re
import struct
import subprocess
import tempfile

binary = Path('target/release/attesor').resolve()
env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
with tempfile.TemporaryDirectory(prefix='weighted-layout-', dir='/private/tmp') as td:
    root = Path(td)
    guest, native = root/'probe', root/'native'
    for arch, dest in [('arm64', guest), ('x86_64', native)]:
        subprocess.run(['clang', '-O2', '-arch', arch, str(Path(__file__).with_name('offline.c')), '-o', str(dest)], check=True, capture_output=True)
    expected = subprocess.run([native], check=True, capture_output=True).stdout
    cache, samples = root/'cache', root/'samples'
    base = dict(env, ATTESOR_CACHE=str(cache), ATTESOR_STATS='1', ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0')
    def run(prewarm=False, on='', off=''):
        r = subprocess.run([binary, *(['--prewarm'] if prewarm else []), guest],
            env=dict(base, ATTESOR_JIT_ON=on, ATTESOR_JIT_OFF=off, ATTESOR_AOT_LAYOUT_PROFILE=str(samples)), capture_output=True, timeout=180)
        assert r.returncode == 0, r.stderr.decode()
        if not prewarm:
            assert r.stdout == expected
        return r.stderr.decode()
    def record():
        p, = cache.glob('main-*.aot')
        b = p.read_bytes()
        n, = struct.unpack_from('<Q', b, 16)
        return p.stem, b[8:12], list(struct.iter_unpack('<QQ', b[40:40+16*n]))
    run(True)
    key, default_id, original = record()
    data = guest.read_bytes()
    ncmds, = struct.unpack_from('<I', data, 16)
    lo, hi, offset = (1 << 64)-1, 0, 32
    for _ in range(ncmds):
        cmd, size = struct.unpack_from('<II', data, offset)
        if cmd == 0x19 and data[offset+8:offset+24].rstrip(b'\0') != b'__PAGEZERO':
            va, sz = struct.unpack_from('<QQ', data, offset+24)
            lo, hi = min(lo, va), max(hi, va+sz)
        offset += size
    target = original[-1][0]
    samples.write_text(f'Attesor AOT layout samples v1\nimage {key} {hi-lo}\npc 0x{target-0x500000000:x} 9\nend\n')
    on = 'offline-layout-profile'
    assert 'aot weighted layout:' in run(True, on)
    _, first_id, weighted = record()
    assert first_id != default_id
    assert weighted[0][0] == target and weighted != original, 'no-op layout mutation must fail'
    assert {p for p, _ in weighted} == {p for p, _ in original}
    assert re.search(r'aot: loaded [1-9]\d* blocks for probe\n', run(False, on))
    # Replacing contents at exactly the same path invalidates the old header.
    samples.write_text(samples.read_text().replace(' 9\n', ' 19\n'))
    assert not re.search(r'aot: loaded [1-9]\d* blocks for probe\n', run(False, on))
    run(True, on)
    assert record()[1] != first_id
    assert 'aot weighted layout:' not in run(True, on, on)
    assert record()[2] == original, 'OFF must preserve default placement'
    samples.write_text('malformed')
    assert 'AOT layout profile ignored' in run(True, on)
    assert record()[2] == original
    print('PASS weighted placement changes real index/order, preserves entry set and execution; content identity, OFF and malformed fallback')
