#!/usr/bin/env python3
"""Headless track-P measurements, alternating OFF/ON with isolated cold caches.

No GUI, no daemon, no user cache. Results include raw wall/child CPU seconds,
uptime, process profiles, logs, and per-process translated blocks/elapsed ms.
JIT translation ms is elapsed time (NOT translation CPU time). CPU seconds cover
all waited-for children in the run. Failed runs remain recorded and fail the tool.
"""
import argparse
import json
import os
from pathlib import Path
import resource
import signal
import subprocess
import time

REPO = Path(__file__).resolve().parent.parent
DEFAULT_APP = '/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness'


def profiles(directory):
    rows = []
    for path in sorted(directory.glob('*.txt')):
        row = {'file': path.name}
        for line in path.read_text().splitlines():
            fields = line.split('\t')
            if fields[0] == 'translation':
                row.update(blocks=int(fields[1]), translate_elapsed_ms=int(fields[2]))
        rows.append(row)
    return rows


def run(binary, argv, env, dest, timeout, background=None):
    dest.mkdir(parents=True)
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    children = []
    background_elapsed = None
    with (dest/'stdout').open('w') as out, (dest/'stderr').open('w') as err, \
         (dest/'background.stdout').open('w') as bgout, (dest/'background.stderr').open('w') as bgerr:
        try:
            if background:
                worker_env = {**env, 'ATTESOR_CACHE': str(dest/'background-cache')}
                worker = subprocess.Popen([str(binary), *map(str, background)], env=worker_env,
                                          stdout=bgout, stderr=bgerr, start_new_session=True)
                children.append(worker)
            child = subprocess.Popen([str(binary), *map(str, argv)], env=env,
                                     stdout=out, stderr=err, start_new_session=True)
            children.append(child)
            rc = child.wait(timeout=timeout)
            foreground_elapsed = time.monotonic()-started
            if background:
                worker_rc = worker.wait(timeout=timeout)
                # Observation includes time spent waiting for the foreground;
                # this is not a precise independent worker-latency measurement.
                background_elapsed = time.monotonic()-started
                if worker_rc:
                    rc = worker_rc
        except BaseException:
            # Exclusively the process groups created by this invocation.
            for owned in children:
                if owned.poll() is None:
                    try:
                        os.killpg(owned.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                owned.wait()
            raise
    elapsed = time.monotonic()-started
    end = resource.getrusage(resource.RUSAGE_CHILDREN)
    return {'wall_s': elapsed, 'foreground_wall_s': foreground_elapsed,
            'background_observed_wall_s': background_elapsed,
            'cpu_s': end.ru_utime+end.ru_stime-usage.ru_utime-usage.ru_stime,
            'returncode': rc, 'uptime': subprocess.check_output(['uptime'], text=True).strip(),
            'processes': profiles(dest/'profile')}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', type=Path, default=REPO/'target/release/attesor')
    p.add_argument('--electron', type=Path, default=Path(DEFAULT_APP))
    p.add_argument('--output', type=Path, required=True, help='new directory within this worktree or /private/tmp')
    p.add_argument('--pairs', type=int, default=3)
    p.add_argument('--feature', default='process-qos')
    p.add_argument('--workloads', nargs='+', choices=['trivial', 'node', 'fork', 'prewarm', 'contention', 'js', 'tls'],
                   default=['trivial', 'node', 'fork', 'prewarm'])
    p.add_argument('--timeout', type=float, default=300)
    args = p.parse_args()
    root = args.output.resolve()
    if not (root.is_relative_to(REPO) or root.is_relative_to(Path('/private/tmp'))):
        p.error('output must be private to this worktree or /private/tmp')
    if args.pairs < 3:
        p.error('at least three alternating pairs are required')
    root.mkdir(parents=True, exist_ok=False)
    # Do not inherit shims, profilers, or a user cache from the invoking shell.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith('ATTESOR_') and k != 'DYLD_INSERT_LIBRARIES'}
    env.update(TMPDIR='/private/tmp', ELECTRON_RUN_AS_NODE='1')
    source, trivial = root/'trivial.c', root/'trivial'
    source.write_text('int main(void) { return 0; }\n')
    subprocess.run(['clang', '-O2', '-arch', 'arm64', str(source), '-o', str(trivial)], env=env, check=True)
    commands = {
        'trivial': [trivial], 'node': [args.electron, '-e', '0'],
        'fork': [args.electron, REPO/'tests/attesor/process-fork.js'],
        'prewarm': ['--prewarm', trivial],
        'contention': [args.electron, '-e', '0'],
        'js': [args.electron, REPO/'docs/tasks/phase2/js.js'],
        'tls': [args.electron, REPO/'docs/tasks/phase2/t.js'],
    }
    with (root/'measurements.jsonl').open('w') as result:
        for pair in range(args.pairs):
            for workload in args.workloads:
                for variant in ['off', 'on']:
                    dest = root/f'{pair}-{workload}-{variant}'
                    run_env = {**env, 'ATTESOR_CACHE': str(dest/'cache'),
                               'ATTESOR_PROFILE': str(dest/'profile'), 'ATTESOR_STATS': '1',
                               'ATTESOR_DSC_STATS': '1', 'ATTESOR_AOT_CHECKPOINT': '0'}
                    # No detached exit builders: the measurement must include all
                    # work and must not leave any workers running after return.
                    if workload != 'prewarm':
                        run_env['ATTESOR_NO_AOT_BUILD'] = '1'
                    if variant == 'on':
                        run_env['ATTESOR_JIT_ON'] = args.feature
                    background = ['--prewarm', trivial] if workload == 'contention' else None
                    row = run(args.attesor.resolve(), commands[workload], run_env, dest, args.timeout, background)
                    row.update(pair=pair, workload=workload, variant=variant)
                    result.write(json.dumps(row)+'\n')
                    result.flush()
                    print(json.dumps(row), flush=True)
                    if row['returncode']:
                        raise SystemExit(f'failed run: {dest}')
                    expected = 7 if workload == 'fork' else 0 if workload == 'prewarm' else 1
                    if len(row['processes']) != expected:
                        raise SystemExit(f'incomplete per-process profiles: {dest}: {row}')
                    output = (dest/'stdout').read_text()
                    if workload == 'fork' and 'PASS fork: 6 children, env argv IPC exit' not in output:
                        raise SystemExit(f'fork check failed: {dest}')
                    if workload == 'tls' and 'https status 401' not in output:
                        raise SystemExit(f'TLS not verified (network may be unavailable): {dest}')


if __name__ == '__main__':
    main()
