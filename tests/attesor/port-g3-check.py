#!/usr/bin/env python3
"""Fresh-process port gates and destructive-in-memory/source-restored sensitivity controls.
No shared app cache, Git writes or performance claims. Run serially, not beside a build.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / 'target/port-g3'
TOKENS = {'10': 'bliccache', '18': 'ibtc2', '20': 'rel32chain', '6': 'notbi'}

def run(name, args, on='', off='', fail=False, **extra):
    env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_')}
    env.update(ATTESOR_JIT_ON=on, ATTESOR_JIT_OFF=off,
               ATTESOR_CACHE=str(OUT / 'cache'), **extra)
    log = OUT / 'logs' / (name + '.log'); log.parent.mkdir(parents=True, exist_ok=True)
    with log.open('w') as f:
        r = subprocess.run(args, cwd=ROOT, env=env, stdout=f, stderr=subprocess.STDOUT, timeout=300)
    text = log.read_text()
    if fail:
        assert r.returncode != 0 and 'test result: FAILED' in text, (name, r.returncode, text[-3000:])
    else:
        assert r.returncode == 0, (name, r.returncode, text[-5000:])
        if '--selftest-jit' in args:
            assert re.search(r'mismatch=0\b', text), text[-1000:]
            assert int(re.search(r'\bok=(\d+)', text).group(1)) > 0, text[-1000:]
        if 'test' in args and 'cargo' in args:
            assert re.search(r'test result: ok\. [1-9]\d* passed', text), text[-1000:]
    print(('SENSITIVE ' if fail else 'PASS ') + name, flush=True)
    return text

def test(name, filt, on='', off='', **extra):
    return run(name, ['cargo', 'test', '--release', '-p', 'attesor', filt,
                     '--', '--test-threads=1', '--nocapture'], on, off, **extra)

MUTATIONS = {
    '6': ('crates/attesor/src/jit/notbi.rs',
          'address.checked_add(m.bytes - 1)',
          'address.checked_add(0) /* mutant: ignore access end */'),
    '20': ('crates/attesor/src/jit/rel32chain.rs',
           'a.jne(body).unwrap();',
           'a.jmp(body).unwrap(); // mutant: ignore revoked alive cell'),
    '18': ('crates/attesor/src/jit/emit.rs',
           'a!(self, jmp(qword_ptr(rcx + crate::jit::IBTC_ENTRY_BYTES + 8)));',
           'a!(self, jmp(l_miss)); // mutant: disable way 1'),
    '10': ('crates/attesor/src/jit/emit.rs',
           'a!(self, cmp(rdx, qword_ptr(rcx + cache::SITE)));\n        a!(self, jne(l_miss));',
           'a!(self, cmp(rdx, qword_ptr(rcx + cache::SITE))); // mutant: omit full site guard'),
}

def main():
    p = argparse.ArgumentParser(); p.add_argument('feature', choices=[*TOKENS, 'all'])
    p.add_argument('--sensitivity', action='store_true'); a = p.parse_args()
    features = list(TOKENS) if a.feature == 'all' else [a.feature]
    for n in features:
        run(n + '-build', ['cargo', 'build', '--release', '-p', 'attesor'])
        token = TOKENS[n]; filt = 'g3_' + token
        for label, on, off in [('default', '', ''), ('on', token, ''), ('off', token, token),
                               ('exact', 'x' + token + ', ' + token, ''),
                               ('combined', token + ',tiered,fastenc,preciseinvalidate,slotreuse,jitcost', '')]:
            test(n + '-' + label, filt, on, off, ATTESOR_JIT_TIER_THRESHOLD='3')
        if n == '10':
            test('10-callret', filt, token + ',callret')
        for cls in (['branch', 'calls'] if n in ('10', '18', '20') else ['tbiblocks', 'blocks', 'loops']):
            run(n + '-selftest-' + cls, [str(ROOT / 'target/release/attesor'), '--selftest-jit', cls, '1000', '0x1234'], token)
        if a.sensitivity:
            path, old, new = MUTATIONS[n]; path = ROOT / path; source = path.read_text()
            assert source.count(old) == 1, (path, old)
            try:
                path.write_text(source.replace(old, new))
                test(n + '-mutant', filt, token, fail=True)
            finally:
                path.write_text(source)
            test(n + '-restored', filt, token)
    if a.feature == 'all':
        all_on = ','.join(TOKENS.values())
        test('g3-all', 'g3_', all_on)
        test('g3-all-callret', 'g3_', all_on + ',callret')
        test('g3-wave1-combined', '', all_on + ',tiered,bgtrans,fastenc,preciseinvalidate,slotreuse,jitcost',
             ATTESOR_JIT_TIER_THRESHOLD='3')
        test('g3-all-off', 'g3_', all_on, all_on)
        test('g3-verify-bliccache', 'g3_bliccache', all_on, ATTESOR_JIT_VERIFY='1')
        test('g3-notbi-old-shifts', 'g3_notbi', 'notbi', 'tbimask')
        for cls in ['tbiblocks', 'blocks', 'fpblocks', 'loops', 'calls', 'jitcode']:
            run('g3-all-selftest-' + cls,
                [str(ROOT / 'target/release/attesor'), '--selftest-jit', cls, '1000', '0x1234'], all_on)
        ids = {}
        for label, on, off in [('default', '', ''), *[(t, t, '') for t in TOKENS.values()],
                               ('all', ','.join(TOKENS.values()), ''),
                               ('off', ','.join(TOKENS.values()), ','.join(TOKENS.values()))]:
            s = test('g3-identity-' + label, 'port_g2_codegen_identity', on, off)
            ids[label] = re.search(r'PORT_G2_ID=([0-9a-f]+)', s).group(1)
        assert len(set(ids.values())) == len(ids), ids
        print('PASS generated-code/AOT identities:', ids)

if __name__ == '__main__': main()
