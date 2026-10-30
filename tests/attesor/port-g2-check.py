#!/usr/bin/env python3
"""Opt-in port directed gates in fresh processes (no user AOT cache)."""
import os
import pathlib
import subprocess

ROOT = pathlib.Path(__file__).resolve().parents[2]
BASE = ROOT / 'target' / 'port-g2'

def run(name, args, on='', off='', **settings):
    env = os.environ.copy()
    for key in ('ATTESOR_JIT_ON', 'ATTESOR_JIT_OFF', 'ATTESOR_JIT_VERIFY',
                'ATTESOR_LOOPRA_BISECT', 'ATTESOR_LOOPRA_LOG', 'ATTESOR_LOOPRA_ALLOW',
                'ATTESOR_JIT_TIER_THRESHOLD'):
        env.pop(key, None)
    env.update(ATTESOR_JIT_ON=on, ATTESOR_JIT_OFF=off,
               ATTESOR_CACHE=str(BASE / 'cache-directed'), **settings)
    path = BASE / 'logs' / f'{name}.log'
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('w') as log:
        subprocess.run(args, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    print(f'PASS {name}', flush=True)

if __name__ == '__main__':
    import argparse
    p = argparse.ArgumentParser()
    p.add_argument('feature', choices=['11', '12', '13', 'all'], default='all', nargs='?')
    feature = p.parse_args().feature
    test = ['cargo', 'test', '--release', '-p', 'attesor']
    end = ['--', '--test-threads=1', '--nocapture']
    if feature in ('11', 'all'):
        for name, on, off in [('on', 'fastenc', ''), ('default', '', ''),
                              ('off', 'fastenc', 'fastenc'), ('exact', 'xfastenc, fastenc', '')]:
            run('11-directed-' + name, test + ['fastenc'] + end, on, off)
    if feature in ('12', 'all'):
        for name, on, off, extra in [
            ('on', 'tiered', '', {'ATTESOR_JIT_TIER_THRESHOLD': '3'}),
            ('default', '', '', {}), ('off', 'tiered', 'tiered', {}),
            ('exact', 'xtiered, tiered', '', {}),
            ('zero', 'tiered', '', {'ATTESOR_JIT_TIER_THRESHOLD': '0'}),
            ('invalid', 'tiered', '', {'ATTESOR_JIT_TIER_THRESHOLD': '4294967296'}),
            ('verify', 'tiered', '', {'ATTESOR_JIT_VERIFY': '1'}),
            ('combined', 'tiered,fastenc,preciseinvalidate,slotreuse,jitcost', '',
             {'ATTESOR_JIT_TIER_THRESHOLD': '3'})]:
            run('12-directed-' + name, test + ['directed_promotion_smc_indirect_and_generation'] + end,
                on, off, **extra)
    if feature in ('13', 'all'):
        for name, on, off, extra in [
            ('on', 'bgtrans', '', {}), ('default', '', '', {}),
            ('off', 'bgtrans', 'bgtrans', {}), ('exact', 'xbgtrans, bgtrans', '', {}),
            ('verify', 'bgtrans', '', {'ATTESOR_JIT_VERIFY': '1'}),
            ('diagnostic', 'bgtrans', '', {'ATTESOR_LOOPRA_LOG': '1'}),
            ('census', 'bgtrans,loopcensus', '', {}),
            ('combined', 'bgtrans,tiered,fastenc,preciseinvalidate,slotreuse,jitcost', '', {})]:
            run('13-directed-' + name, test + ['bgtrans'] + end, on, off, **extra)

    if feature == 'all':
        import re
        identities = {}
        for name, on, off in [('default', '', ''), ('fastenc', 'fastenc', ''),
                              ('tiered', 'tiered', ''), ('bgtrans', 'bgtrans', ''),
                              ('all', 'fastenc,tiered,bgtrans', ''),
                              ('off', 'fastenc,tiered,bgtrans', 'fastenc,tiered,bgtrans')]:
            run('identity-' + name, test + ['port_g2_codegen_identity'] + end, on, off)
            text = (BASE / 'logs' / ('identity-' + name + '.log')).read_text()
            identities[name] = re.search(r'PORT_G2_ID=([0-9a-f]+)', text).group(1)
        assert len(set(identities.values())) == len(identities), identities
        print('PASS independent AOT identities:', identities)
