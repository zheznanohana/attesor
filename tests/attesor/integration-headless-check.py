#!/usr/bin/env python3
"""Headless Electron Node/crypto/TLS parity for every integration opt-in.

No installed files or application data are modified. Network denial is an
explicit UNVERIFIED result, not a TLS success. Only owned processes are killed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import runpy
import shutil
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
TOKENS = runpy.run_path(str(Path(__file__).with_name('integration-check.py')))['TOKENS']
CRYPTO = ['sha256 ca6073392ee71dbd', 'sha512 fcf47d64f5ceadd9',
          'gcm 89788ae8c19a5009 tag de047988105bcc38', 'ecdsa verify true', 'ecdh true',
          'string 13 e5b7a6e4b88ae8a792e9a1b6e6a08f']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT/'target/integ1/headless')
    parser.add_argument('--electron', type=Path,
                        default=Path('/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness'))
    parser.add_argument('--configs', nargs='+', default=['default', *TOKENS, 'all'])
    parser.add_argument('--js-only', action='store_true',
                        help='compare the unchanged COMMON.md seven-kernel JS workload with native Node')
    parser.add_argument('--timeout', type=float, default=600,
                        help='per-process limit; combined census/tiering/worker diagnostics are expensive')
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    out = args.output.resolve()
    if not out.is_relative_to(ROOT/'target'):
        parser.error('--output must be under this worktree target/')
    out.mkdir(parents=True, exist_ok=True)
    if not args.electron.is_file():
        (out/'summary.json').write_text(json.dumps({'status': 'SKIPPED', 'reason': 'Electron binary unavailable'})+'\n')
        print('SKIPPED Electron binary unavailable')
        return
    base = {k: v for k, v in os.environ.items() if not k.startswith(('ATTESOR_', 'DYLD_'))}
    base.update(TMPDIR='/private/tmp', ELECTRON_RUN_AS_NODE='1',
                ATTESOR_CACHE=str(out/'cache'), ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0',
                INTEG_NODE_TRACE='1')
    rows = []
    identity = hashlib.sha256((ROOT/'target/release/attesor').read_bytes()).hexdigest()

    def run(label, kind, command, flags='', off=''):
        started = time.monotonic()
        env = dict(base, ATTESOR_JIT_ON=flags, ATTESOR_JIT_OFF=off)
        file_label = label if len(label) < 100 else 'tokens-' + hashlib.sha256(label.encode()).hexdigest()[:12]
        error_path = out/f'{file_label}-{kind}.stderr.log'
        expired = False
        with error_path.open('w') as error:
            with subprocess.Popen(list(map(str, command)), cwd=ROOT, env=env, text=True,
                                  stdout=subprocess.PIPE, stderr=error, start_new_session=True) as proc:
                (out/'active.json').write_text(json.dumps({'pid': proc.pid, 'config': label, 'kind': kind})+'\n')
                try:
                    stdout, _ = proc.communicate(timeout=args.timeout)
                    rc = proc.returncode
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    stdout, _ = proc.communicate()
                    rc, expired = -signal.SIGKILL, True
        stderr = error_path.read_text(errors='replace')
        if expired:
            stderr += f'\nTIMEOUT after {args.timeout:g} seconds\n'
        (out/f'{file_label}-{kind}.log').write_text(stdout+'\n--- stderr ---\n'+stderr)
        error_path.unlink()
        (out/'active.json').unlink()
        row = dict(config=label, kind=kind, on=flags, off=off, returncode=rc, binary_sha256=identity,
                   seconds=round(time.monotonic()-started, 3), status='PASS' if rc == 0 else 'FAIL')
        rows.append(row)
        shutil.rmtree(out/'cache', ignore_errors=True)
        return stdout, row

    if args.js_only:
        script = ROOT/'docs/tasks/phase2/js.js'
        def values(stdout):
            result = dict(re.findall(r'^(loops|objects|json|regex|strings|closures|sort)\s+\d+\s+ms\s+(.+)$', stdout, re.M))
            return result if len(result) == 7 else None
        stdout, row = run('native', 'js', [args.electron, script])
        reference = values(stdout)
        if reference is None: row['status'] = 'FAIL'
        row['values'] = reference
        print(row, flush=True)
        for label in args.configs:
            flags = '' if label == 'default' else ','.join(TOKENS) if label in ('all', 'all-off') else label
            stdout, row = run(label, 'js', [ROOT/'target/release/attesor', args.electron, script],
                              flags, flags if label == 'all-off' else '')
            row['values'] = values(stdout)
            if reference is None or row['values'] != reference: row['status'] = 'FAIL'
            print(row, flush=True)
            (out/'summary.json').write_text(json.dumps(rows, indent=2)+'\n')
        failed = sum(row['status'] == 'FAIL' for row in rows)
        print(f'RESULT {len(rows)-failed} passed; {failed} failed', flush=True)
        raise SystemExit(bool(failed))

    node = ROOT/'tests/attesor/electron-node-probe.cjs'
    stdout, native = run('native', 'node', [args.electron, node])
    reference = None
    if native['status'] == 'PASS':
        try:
            reference = json.loads(stdout.strip())
        except ValueError:
            native['status'] = 'FAIL'
    print(native, flush=True)
    for label in args.configs:
        flags = '' if label == 'default' else ','.join(TOKENS) if label in ('all', 'all-off') else label
        off = flags if label == 'all-off' else ''
        command = [ROOT/'target/release/attesor', args.electron]
        stdout, row = run(label, 'node', [*command, node], flags, off)
        try:
            if reference is None or json.loads(stdout.strip()) != reference:
                row['status'] = 'FAIL'
        except ValueError:
            row['status'] = 'FAIL'
        print(row, flush=True)
        stdout, row = run(label, 'tls', [*command, ROOT/'docs/tasks/phase2/t.js'], flags, off)
        if not all(value in stdout.splitlines() for value in CRYPTO):
            row['status'] = 'FAIL'
        row['https'] = 'PASS' if 'https status 401' in stdout.splitlines() else 'UNVERIFIED'
        row['network_result'] = next((line for line in stdout.splitlines() if line.startswith('https ')), 'no HTTPS result')
        if row['https'] == 'UNVERIFIED' and row['status'] == 'PASS' and not re.search(r'^https error ', stdout, re.M):
            row['status'] = 'FAIL'
        print(row, flush=True)
        # Persist progress even if interrupted during a later configuration.
        (out/'summary.json').write_text(json.dumps(rows, indent=2)+'\n')
    failed = sum(row['status'] == 'FAIL' for row in rows)
    print(f'RESULT {len(rows)-failed} passed; {failed} failed; '
          f'{sum(row.get("https") == "UNVERIFIED" for row in rows)} HTTPS unverified', flush=True)
    if failed:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
