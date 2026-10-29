#!/usr/bin/env python3
"""Measure sandbox container redirection, with a signed private CLI guest only.

Alternate disabled/enabled runs (>=3 pairs), reusing only a private pre-slid
cache. Each run gets a fresh private HOME and profile. Does not apply a host
sandbox or launch an application. Requires the ARM dyld shared-cache sysroot.
"""
import argparse
import json
import os
from pathlib import Path
import resource
import subprocess
import time

REPO = Path(__file__).resolve().parent.parent
IDENTIFIER = 'org.attesor.trackp.private'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', type=Path, default=REPO/'target/release/attesor')
    p.add_argument('--sysroot', type=Path, required=True)
    p.add_argument('--output', type=Path, required=True, help='new directory in this worktree or /private/tmp')
    p.add_argument('--pairs', type=int, default=3)
    args = p.parse_args()
    root = args.output.resolve()
    if not (root.is_relative_to(REPO) or root.is_relative_to(Path('/private/tmp'))):
        p.error('output must be private to this worktree or /private/tmp')
    if args.pairs < 3:
        p.error('at least three alternating pairs are required')
    if not (args.sysroot/'dyld_shared_cache_arm64e').is_file():
        p.error('sysroot must contain dyld_shared_cache_arm64e')
    root.mkdir(parents=True, exist_ok=False)
    source, guest, ent = root/'probe.c', root/'probe', root/'entitlements.plist'
    source.write_text('int main(void) { return 0; }\n')
    ent.write_text('<?xml version="1.0"?><plist version="1.0"><dict>'
                   '<key>com.apple.security.app-sandbox</key><true/></dict></plist>')
    env = {k: v for k, v in os.environ.items()
           if not k.startswith('ATTESOR_') and k != 'DYLD_INSERT_LIBRARIES'}
    env.update(TMPDIR='/private/tmp', ATTESOR_NO_AOT_BUILD='1',
               ATTESOR_AOT_CHECKPOINT='0', ATTESOR_CACHE=str(root/'cache'))
    subprocess.run(['clang', '-O2', '-arch', 'arm64', str(source), '-o', str(guest)], env=env, check=True)
    subprocess.run(['codesign', '-f', '-s', '-', '-i', IDENTIFIER,
                    '--entitlements', str(ent), str(guest)], env=env, check=True)
    cmd = [str(args.attesor.resolve()), '--sysroot', str(args.sysroot.resolve()), str(guest)]
    seed = root/'seed-home'
    seed.mkdir()
    subprocess.run(cmd, env={**env, 'HOME': str(seed), 'ATTESOR_NO_CONTAINER': '1'},
                   check=True, capture_output=True, timeout=120)
    with (root/'measurements.jsonl').open('w') as log:
        for pair in range(args.pairs):
            for enabled in [False, True]:
                name = f'{pair}-' + ('on' if enabled else 'off')
                home = root/(name+'-home')
                home.mkdir()
                run_env = {**env, 'HOME': str(home), 'ATTESOR_PROFILE': str(root/(name+'-profile'))}
                if not enabled:
                    run_env['ATTESOR_NO_CONTAINER'] = '1'
                before = resource.getrusage(resource.RUSAGE_CHILDREN)
                start = time.monotonic()
                result = subprocess.run(cmd, env=run_env, text=True, capture_output=True, timeout=120)
                elapsed = time.monotonic()-start
                after = resource.getrusage(resource.RUSAGE_CHILDREN)
                (root/(name+'.stdout')).write_text(result.stdout)
                (root/(name+'.stderr')).write_text(result.stderr)
                mapped = (home/f'Library/Containers/{IDENTIFIER}/Data').is_dir()
                row = {'pair': pair, 'container': enabled, 'wall_s': elapsed,
                       'cpu_s': after.ru_utime+after.ru_stime-before.ru_utime-before.ru_stime,
                       'returncode': result.returncode, 'mapped': mapped,
                       'uptime': subprocess.check_output(['uptime'], text=True).strip()}
                log.write(json.dumps(row)+'\n')
                log.flush()
                print(json.dumps(row), flush=True)
                if result.returncode or mapped != enabled:
                    raise SystemExit(f'container probe failed: {name}: {result.stderr}')


if __name__ == '__main__':
    main()
