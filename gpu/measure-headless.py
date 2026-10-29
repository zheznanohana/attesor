#!/usr/bin/env python3
"""Fail-closed, no-window Metal baseline or alternating before/after measurement.

Outputs are private to a new directory. Existing bridge libraries are never rebuilt,
installed or overwritten. Submitted upload bytes are NOT bridge memcpy counters.
"""
import argparse
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def parse_result(stdout):
    result = re.findall(r'^RESULT mode=(gpu|oracle) frames=(\d+) seed=(\d+) '
                        r'checksum=([0-9a-f]{16}) checked_bytes=(\d+) mismatch=0$', stdout, re.M)
    if len(result) != 1:
        raise ValueError('missing or ambiguous validated RESULT')
    mode, frames, seed, checksum, checked = result[0]
    return dict(mode=mode, frames=int(frames), seed=int(seed), checksum=checksum, checked_bytes=int(checked))


def parse_measure(stdout):
    rows = re.findall(r'^MEASURE seconds=([0-9.]+) frames_per_second=([0-9.]+) '
                      r'command_buffers=(\d+) submitted_upload_bytes=(\d+)$', stdout, re.M)
    if len(rows) != 1:
        raise ValueError('missing or ambiguous MEASURE')
    seconds, fps, commands, uploads = rows[0]
    seconds, fps = float(seconds), float(fps)
    if not 0 < seconds < float('inf') or not 0 < fps < float('inf'):
        raise ValueError('nonpositive/nonfinite measurement')
    return dict(seconds=seconds, frames_per_second=fps, command_buffers=int(commands),
                submitted_upload_bytes=int(uploads))


def parse_upload_counters(stderr):
    rows = re.findall(r'^\[mclupload\] snapshot_bytes=(\d+) borrowed_bytes=(\d+) inline_bytes=(\d+)$', stderr, re.M)
    if not rows:
        return None  # Old baselines have no counters; do not interpret missing as zero.
    return dict(zip(('snapshot_bytes', 'borrowed_bytes', 'inline_bytes'),
                    (sum(int(row[i]) for row in rows) for i in range(3))))


def equivalent(a, b):
    return all(a[key] == b[key] for key in ('frames', 'seed', 'checksum', 'checked_bytes'))


def clean_env(output, sysroot):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(('ATTESOR_', 'MCL_', 'DYLD_'))}
    env.update(TMPDIR=str(output / 'tmp'), ATTESOR_CACHE=str(output / 'cache'),
               ATTESOR_NO_AOT_BUILD='1')
    if sysroot:
        env['ATTESOR_SYSROOT'] = str(Path(sysroot).resolve())
    return env


def run(command, env, directory, timeout):
    directory.mkdir()
    (directory / 'command.json').write_text(json.dumps(list(map(str, command)), indent=2) + '\n')
    uptime = subprocess.run(['uptime'], text=True, capture_output=True, check=True).stdout.strip()
    (directory / 'uptime.txt').write_text(uptime + '\n')
    start = time.monotonic()
    try:
        p = subprocess.run(list(map(str, command)), env=env, cwd=ROOT, text=True,
                           capture_output=True, timeout=timeout)
        stdout, stderr, code = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        stdout, stderr, code = e.stdout or b'', e.stderr or b'', 124
        if isinstance(stdout, bytes): stdout = stdout.decode(errors='replace')
        if isinstance(stderr, bytes): stderr = stderr.decode(errors='replace')
        stderr += '\nTIMEOUT: no valid measurement\n'
    (directory / 'stdout.txt').write_text(stdout)
    (directory / 'stderr.txt').write_text(stderr)
    record = dict(returncode=code, wall_seconds=time.monotonic() - start, uptime=uptime)
    (directory / 'status.json').write_text(json.dumps(record, indent=2) + '\n')
    return record, stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--attesor', type=Path, default=ROOT / 'target/release/attesor')
    parser.add_argument('--before', type=Path, required=True, help='directory of baseline bridge dylibs')
    parser.add_argument('--after', type=Path, help='optional candidate bridge dylibs; alternates B/A')
    parser.add_argument('--output', type=Path, required=True, help='new directory (never overwritten)')
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--frames', type=int, default=120)
    parser.add_argument('--seed', type=int, default=1)
    parser.add_argument('--timeout', type=int, default=180)
    parser.add_argument('--sysroot', default=os.environ.get('ATTESOR_SYSROOT'))
    args = parser.parse_args()
    if args.runs < 3 or not 1 <= args.frames <= 100000 or not 0 <= args.seed < 2**64 or args.timeout < 1:
        parser.error('runs >=3, frames 1..100000, uint64 seed and positive timeout required')
    variants = [('before', args.before.resolve())]
    if args.after:
        variants.append(('after', args.after.resolve()))
    for _, directory in variants:
        for name in ('libmclmetal.dylib', 'libmclbridge.dylib'):
            if not (directory / name).is_file(): parser.error(f'missing {directory / name}')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    for directory in ('tmp', 'cache'):
        (output / directory).mkdir()
    env = clean_env(output, args.sysroot)
    summary = dict(status='incomplete', variants={k: str(v) for k, v in variants}, runs=[],
                   byte_accounting='submitted_upload_bytes is workload input; upload counters cover fastpath pointer payload only, not framework/driver copies')

    def finish(status, code, reason=None):
        summary['status'] = status
        if reason: summary['reason'] = reason
        (output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(f'{status}: {reason or output}')
        return code

    native, guest = output / 'headless-native', output / 'headless-arm64'
    for arch, binary in [('x86_64', native), ('arm64', guest)]:
        status, _ = run(['clang', '-arch', arch, '-O2', '-Wall', '-Wextra', '-Werror', '-fobjc-arc',
                         '-framework', 'Foundation', '-framework', 'Metal', ROOT / 'gpu/headless.m',
                         '-o', binary], env, output / ('build-' + arch), args.timeout)
        if status['returncode']: return finish('failed', 1, 'workload compilation failed')
    status, _ = run([native, '--check-oracle'], env, output / 'oracle-sensitivity', args.timeout)
    if status['returncode']: return finish('failed', 1, 'byte oracle sensitivity failed')
    # A native no-device result is an unavailable environment, never a passing zero-work run.
    status, _ = run([native, '1', args.seed], env, output / 'native-preflight', args.timeout)
    summary['native_preflight'] = status
    if status['returncode']:
        return finish('unavailable' if status['returncode'] == 77 else 'failed',
                      77 if status['returncode'] == 77 else 1, 'native Metal preflight failed; no baseline or A/B claimed')
    status, stdout = run([native, '--oracle', args.frames, args.seed], env, output / 'oracle', args.timeout)
    if status['returncode']: return finish('failed', 1, 'CPU oracle failed')
    oracle = parse_result(stdout)
    status, stdout = run([native, args.frames, args.seed], env, output / 'native', args.timeout)
    if status['returncode'] or not equivalent(parse_result(stdout), oracle):
        return finish('failed', 1, 'native GPU differs from byte oracle')
    summary['native'] = {**status, **parse_measure(stdout), **parse_result(stdout)}
    for index in range(args.runs):
        for label, bridge in variants:
            directory = output / f'{index + 1:02}-{label}'
            profile = directory / 'profile'
            run_env = dict(env, ATTESOR_GPU_DIR=str(bridge),
                           ATTESOR_GPU_BRIDGE=str(bridge / 'libmclbridge.dylib'),
                           ATTESOR_PROFILE=str(profile), MCL_STATS='1')
            status, stdout = run([args.attesor.resolve(), '--no-aot', '--gpu', guest, args.frames, args.seed],
                                 run_env, directory, args.timeout)
            record = dict(variant=label, **status)
            summary['runs'].append(record)
            if status['returncode']:
                return finish('unavailable' if status['returncode'] == 77 else 'failed',
                              77 if status['returncode'] == 77 else 1, 'guest workload did not finish; see run logs')
            result = parse_result(stdout)
            if result['mode'] != 'gpu' or not equivalent(result, oracle):
                return finish('failed', 1, 'guest GPU differs from byte oracle')
            record.update(parse_measure(stdout), **result)
            record['fastpath_upload_bytes'] = parse_upload_counters((directory / 'stderr.txt').read_text())
            p = subprocess.run([sys.executable, str(ROOT / 'tools/profile-report.py'), str(profile)],
                               env=env, text=True, capture_output=True, timeout=args.timeout)
            (directory / 'profile-report.txt').write_text(p.stdout + p.stderr)
            if p.returncode: return finish('failed', 1, 'GPU profile missing or invalid')
            # Raw per-operation calls and top selectors are retained in profile/report files.
            import importlib.util
            spec = importlib.util.spec_from_file_location('profile_report', ROOT / 'tools/profile-report.py')
            report = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(report)
            totals = report.Report()
            for path in profile.glob('*.txt'): totals.add(report.read_profile(path)[1])
            ops = {str(number): v[2] for (category, number, _), v in totals.calls.items() if category == 'gpu-op'}
            # All hostcalls, including free/event/selector definitions, are round trips.
            record['hostcalls'] = ops
            record['hostcalls_per_frame_including_setup'] = sum(ops.values()) / args.frames
    if args.after:
        ratios = [summary['runs'][i]['seconds'] / summary['runs'][i + 1]['seconds']
                  for i in range(0, len(summary['runs']), 2)]
        summary['before_over_after_seconds'] = ratios
        summary['median_speedup'] = statistics.median(ratios)
    return finish('measured', 0)


if __name__ == '__main__':
    sys.exit(main())
