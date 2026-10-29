#!/usr/bin/env python3
"""Isolated pre-slid cache correctness, corruption, concurrency and startup probe."""
import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', type=Path, default=Path('target/release/attesor'))
    parser.add_argument('--output', type=Path, default=Path('target/dscslide-check'))
    parser.add_argument('--baseline', type=Path)
    args = parser.parse_args()
    binary = args.attesor.resolve()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)  # Never delete or reuse somebody else's cache.
    tmp = root / 'tmp'
    tmp.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
    env.update(TMPDIR=str(tmp), ATTESOR_CACHE=str(root / 'cache'),
               ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0')
    guest = root / 'probe-arm64'
    native = root / 'probe-x86'
    source = Path(__file__).with_name('dscslide.c')
    for arch, exe in [('arm64', guest), ('x86_64', native)]:
        subprocess.run(['clang', '-arch', arch, '-O2', '-framework', 'CoreFoundation',
                        str(source), '-o', str(exe)], env=env, check=True)
    reference = subprocess.check_output([str(native)], env=env)
    rows = []

    def run(label, extra=None, sample=True, aot=False, exe=binary):
        local = {**env, **(extra or {})}
        if sample:
            local.update(ATTESOR_DSC_STATS='1', ATTESOR_DSC_SAMPLE=str(root / (label + '.pages')))
        command = [str(exe), *([] if aot else ['--no-aot']), str(guest)]
        start = time.perf_counter()
        p = subprocess.run(command, env=local, capture_output=True, timeout=120)
        elapsed = time.perf_counter() - start
        (root / (label + '.out')).write_bytes(p.stdout)
        (root / (label + '.err')).write_bytes(p.stderr)
        assert p.returncode == 0 and p.stdout == reference, (label, p.returncode, p.stdout, p.stderr)
        match = re.search(r'dscslide: status=(\w+) (.*)', p.stderr.decode())
        row = dict(label=label, seconds=elapsed, status=match[1] if match else None)
        if match:
            row['metrics'] = {k: float(v) if '.' in v else int(v)
                              for k, v in re.findall(r'(\w+)=([0-9.]+)', match[2])}
        if sample:
            data = (root / (label + '.pages')).read_bytes()
            assert data, label
            assert data == (root / 'disabled.pages').read_bytes(), label + ': slid page mismatch'
            row['sample_sha256'] = hashlib.sha256(data).hexdigest()
        rows.append(row)
        return row

    # Before any guest can mutate data, compare actual bytes in three slid
    # dyld pages per mapping, not just pointers or checksums printed by the guest.
    run('disabled', {'ATTESOR_DSC_DISABLE': '1'})
    assert not list((root / 'cache').glob('*.dscslide'))
    assert run('cold')['status'] == 'built'
    assert run('warm')['status'] == 'hit'
    files = list((root / 'cache').glob('*.dscslide'))
    assert len(files) == 1
    cache = files[0]
    original_mtime = cache.stat().st_mtime
    os.utime(cache, (time.time() - 31*86400,) * 2)
    assert run('touch')['status'] == 'hit'
    assert cache.stat().st_mtime >= original_mtime

    # Corrupt/truncated/version-mismatched artifacts must take the old path and
    # atomically repair the cache while preserving output and sampled bytes.
    for label, offset, value in [('bad-checksum', cache.stat().st_size-1, b'\x7f'),
                                  ('old-version', 8, (999).to_bytes(8, 'little'))]:
        with cache.open('r+b') as f:
            f.seek(offset)
            old = f.read(len(value))
            if old == value:
                value = bytes([value[0] ^ 1]) + value[1:]
            f.seek(offset)
            f.write(value)
        assert run(label)['status'] == 'built'
        assert run(label + '-warm')['status'] == 'hit'
    with cache.open('r+b') as f:
        f.truncate(100)
    assert run('truncated')['status'] == 'built'
    assert run('truncated-warm')['status'] == 'hit'

    # A locked writer does not block startup and a bad cache directory is not fatal.
    lock_dir = root / 'locked'
    lock_dir.mkdir()
    with (lock_dir / cache.with_suffix('.dscslide.lock').name).open('wb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert run('locked', {'ATTESOR_CACHE': str(lock_dir)})['status'] == 'fallback'
        assert not list(lock_dir.glob('*.dscslide'))
    blocked = root / 'not-a-directory'
    blocked.write_text('cache failure fixture')
    assert run('io-fallback', {'ATTESOR_CACHE': str(blocked)})['status'] == 'fallback'

    concurrent_dir = root / 'concurrent'
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        concurrent_rows = list(pool.map(lambda i: run(f'concurrent-{i}',
            {'ATTESOR_CACHE': str(concurrent_dir)}), range(4)))
    assert sum(r['status'] == 'built' for r in concurrent_rows) == 1, concurrent_rows
    assert len(list(concurrent_dir.glob('*.dscslide'))) == 1
    assert not list(concurrent_dir.glob('*.tmp.*'))
    assert run('concurrent-warm', {'ATTESOR_CACHE': str(concurrent_dir)})['status'] == 'hit'

    # Also exercise the normal AOT-enabled startup path, without building code.
    aot_dir = root / 'with-aot'
    assert run('aot-cold', {'ATTESOR_CACHE': str(aot_dir)}, aot=True)['status'] == 'built'
    assert run('aot-warm', {'ATTESOR_CACHE': str(aot_dir)}, aot=True)['status'] == 'hit'
    print('PASS native/disabled/cold/warm output and sampled page bytes; checksum/version/truncation fallback; touch; lock/I/O fallback; four starters/one writer; AOT coexistence', flush=True)

    # No diagnostic sampling or profiling in wall-time runs. "Cold" means no
    # pre-slid artifact, not a flushed OS page cache (never flush the user's cache).
    modes = ['disabled', 'warm', 'cold']
    if args.baseline:
        modes.insert(0, 'original')
    times = {mode: [] for mode in modes}
    # Interleave and rotate ordering to reduce systematic host-load/thermal
    # drift between the before/after groups. Retain every sample, not the best.
    for i in range(5):
        order = modes[i % len(modes):] + modes[:i % len(modes)]
        for mode in order:
            if mode == 'cold':
                cache.unlink()
            extra = {'ATTESOR_DSC_DISABLE': '1'} if mode == 'disabled' else {}
            exe = args.baseline.resolve() if mode == 'original' else binary
            times[mode].append(run(f'{mode}-time-{i}', extra, sample=False, exe=exe)['seconds'])
    report = dict(passed=True, output=reference.decode().strip(), file_bytes=cache.stat().st_size,
                  sample_bytes=(root / 'disabled.pages').stat().st_size, runs=rows, timings=times,
                  medians={key: statistics.median(value) for key, value in times.items()},
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    (root / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({k: report[k] for k in ['file_bytes', 'sample_bytes', 'timings', 'medians']}, indent=2))
    print('PASS report:', root / 'report.json')


if __name__ == '__main__':
    main()
