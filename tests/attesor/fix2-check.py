#!/usr/bin/env python3
"""FIX2 reproducible serial measurements and source-restoring sensitivity gates.

Run from the workspace root. Timing never runs concurrently within this script.
Test-only file backing is NOT a production POSIX-shm acceptance result.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/fix2'
CLASSES = 'blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks addrforms'.split()


def env_for(token='off'):
    env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
    if token != 'off':
        env['ATTESOR_JIT_ON'] = token
    return env


def measure(args):
    binary = str(Path(args.binary).resolve())
    with (OUT / (args.phase + '.jsonl')).open('w') as log:
        for run in range(args.runs):
            modes = ['off', 'rel32chain', 'sharedtrans']
            if args.file_backing:
                modes = ['off', 'sharedtrans']
            modes = modes[run % len(modes):] + modes[:run % len(modes)]
            for kernel in ['k-arm64', 'b-arm64']:
                for token in modes:
                    env = env_for(token)
                    guest = str(Path(args.kernels, kernel).resolve())
                    if args.file_backing:
                        env['ATTESOR_TEST_KERNEL'] = guest
                        env['ATTESOR_TEST_SHARED_FILE'] = str(OUT / f'{args.phase}-{kernel}.pool')
                        command = [binary, '--exact', 'fix2_kernel_worker', '--nocapture', '--test-threads=1']
                    else:
                        command = [binary, '--no-aot', guest]
                    start = time.monotonic()
                    p = subprocess.run(command, env=env, text=True, capture_output=True, timeout=300)
                    row = dict(run=run + 1, kernel=kernel, mode=token, seconds=time.monotonic() - start,
                               rc=p.returncode, stdout=p.stdout, stderr=p.stderr)
                    log.write(json.dumps(row) + '\n')
                    log.flush()
                    print(json.dumps(row), flush=True)
                    if p.returncode:
                        raise RuntimeError(row)


def selftests(args):
    for token in ['rel32chain', 'sharedtrans']:
        for cls in CLASSES:
            p = subprocess.run([args.binary, '--selftest-jit', cls, '100000'], env=env_for(token),
                               text=True, capture_output=True, timeout=600)
            (OUT / f'selftest-{token}-{cls}.log').write_text(p.stdout + p.stderr)
            print(token, cls, p.returncode, (p.stdout + p.stderr).strip(), flush=True)
            if p.returncode or 'mismatch=0' not in p.stdout + p.stderr:
                raise RuntimeError((token, cls))


def sensitivity(_):
    runtime = ROOT / 'crates/attesor/src/jit/mod.rs'
    shared = ROOT / 'crates/attesor/src/jit/sharedtrans.rs'
    mutants = [
        ('dispatcher-still-refuses-late-links', runtime,
         'if slot != 0 && !verify_mode() {',
         'if slot != 0 && !verify_mode() && !rel32chain::enabled() {',
         'rel32chain', 'fix2_rel32_dispatcher_installs_late_slot'),
        ('disabled-late-links', runtime, 'fn chain_slot(slot: u64, pc: u64, target: u64) {',
         'fn chain_slot(slot: u64, pc: u64, target: u64) { if rel32chain::enabled() { return; }',
         'rel32chain,callret', 'fix2_rel32_forward_call'),
        ('missing-execution-lease', runtime,
         'let _graph = rel32chain::enabled().then(|| chain_graph().read().unwrap());',
         '// MUTANT: execution lease removed', 'rel32chain,callret', 'fix2_rel32_forward_call'),
        ('guard-on-ordinary-entry', runtime,
         'let public = res.label_ip(&u.labels[&pc]).expect("public body") + prefix;',
         'let public = res.label_ip(&guard_label.unwrap()).unwrap() + prefix;', 'rel32chain,callret', 'fix2_rel32_forward_call'),
        ('plain-still-dispatches', shared,
         '''            local[at] = 0xe9;
            local[at+1..at+5].copy_from_slice(&disp.to_le_bytes());
            local[at+5..at+i.len()].fill(0x90);''',
         '            let _ = disp; // MUTANT: keep Plain dispatcher jumps',
         'sharedtrans', 'fix2_shared_plain_exit'),
        ('repeated-attachment', shared, 'POOL.get_or_init(Mapping::open).as_ref()',
         'Box::leak(Box::new(Mapping::open())).as_ref()',
         'sharedtrans', 'fix2_shared_attachment'),
        ('word-at-a-time-snapshot', shared,
         'let page_end = (at | 0xfff).checked_add(1)?.min(end);',
         'let page_end = at.checked_add(4)?.min(end);',
         'sharedtrans', 'fix2_shared_snapshot'),
        ('guard-shifts-hot-layout', runtime,
         '    u.labels.insert(pc, l);\n    let reader = guest_reader();',
         '    u.labels.insert(pc, l);\n    if guard.is_some() { u.a.db(&[0x90]).unwrap(); }\n    let reader = guest_reader();',
         'rel32chain', 'fix2_rel32_guard_preserves_hot_body'),
    ]
    def gate(name, stage, token, test, should_pass):
        p = subprocess.run(['cargo', 'test', '--release', '-p', 'attesor', test, '--', '--nocapture', '--test-threads=1'],
                           env=env_for(token), text=True, capture_output=True)
        output = p.stdout + p.stderr
        (OUT / f'sensitivity-{name}-{stage}.log').write_text(output)
        if should_pass:
            assert p.returncode == 0 and 'test result: ok. 1 passed;' in output, (name, stage, output[-3000:])
        else:
            assert p.returncode != 0 and 'test result: FAILED. 0 passed; 1 failed;' in output, (name, stage, output[-3000:])
        print(name, stage, 'PASS' if should_pass else 'EXPECTED TEST FAILURE', flush=True)
    for name, path, old, new, token, test in mutants:
        pristine = path.read_text()
        assert pristine.count(old) == 1, (name, pristine.count(old))
        gate(name, 'gate', token, test, True)
        try:
            path.write_text(pristine.replace(old, new))
            gate(name, 'mutant', token, test, False)
        finally:
            path.write_text(pristine)
        gate(name, 'restored', token, test, True)


def main():
    os.chdir(ROOT)
    OUT.mkdir(parents=True, exist_ok=True)
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest='command', required=True)
    m = sub.add_parser('measure')
    m.add_argument('--binary', default='target/release/attesor')
    m.add_argument('--kernels', default='target/fix2')
    m.add_argument('--phase', required=True)
    m.add_argument('--runs', type=int, default=3)
    m.add_argument('--file-backing', action='store_true', help='requires cfg(test) test binary, never production')
    m.set_defaults(func=measure)
    s = sub.add_parser('selftests')
    s.add_argument('--binary', default='target/release/attesor')
    s.set_defaults(func=selftests)
    sub.add_parser('sensitivity').set_defaults(func=sensitivity)
    args = p.parse_args()
    args.func(args)


if __name__ == '__main__':
    main()
