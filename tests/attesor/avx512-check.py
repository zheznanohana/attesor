#!/usr/bin/env python3
"""Track B regression matrix. AVX-512 execution requires the OS feature detector;
passing on an unsupported host proves fallback only, not high-bank correctness.
"""
import argparse, json, os, re, subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('attesor', type=Path, nargs='?', default=Path('target/release/attesor'))
p.add_argument('--count', type=int, default=100000)
p.add_argument('--output', type=Path, default=Path('target/avx512/check'))
a=p.parse_args(); root=a.output.resolve(); root.mkdir(parents=True,exist_ok=True)
binary=a.attesor.resolve()
env={k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_')}
env.update(TMPDIR='/private/tmp',ATTESOR_CACHE=str(root/'cache'))
classes='blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks 0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000'.split()
rows=[]
def run(cmd, extra, name, timeout=900):
    x=subprocess.run(list(map(str,cmd)),env={**env,**extra},text=True,capture_output=True,timeout=timeout)
    (root/(name+'.log')).write_text(x.stdout+x.stderr)
    if x.returncode: raise RuntimeError(f'{name}: exit {x.returncode}; see log')
    return x
for flag in ['', 'avx512']:
    label=flag or 'off'; extra={'ATTESOR_JIT_ON':flag}
    for cls in classes:
        seeds=[1,0x12345678,0xfeedbeef] if cls=='loops' else [None]
        for seed in seeds:
            suffix=[] if seed is None else [seed]
            name=label+'-'+cls.replace(':','-')+'-'+str(seed)
            x=run([binary,'--selftest-jit',cls,a.count,*suffix],extra,name)
            assert re.search(r'mismatch=0(?:\s|$)',x.stdout), x.stdout
            assert not re.search(r'MISMATCH',x.stdout),x.stdout
            rows.append({'flag':flag,'class':cls,'seed':seed,'stdout':x.stdout.strip()})
            print(json.dumps(rows[-1]),flush=True)
            (root/'selftests.json').write_text(json.dumps(rows,indent=2)+'\n')
    x=run(['bash','tests/attesor/loopra-aot-check.sh',binary],extra,label+'-loopra-aot')
    print(x.stdout,flush=True)
guest=root/'neon-edge'
run(['clang','-O2','-arch','arm64','tests/attesor/neon_edge.c','-o',guest],{},'neon-build')
expected=run([binary,'--interp','--no-aot',guest],{},'neon-interp').stdout
for flag in ['', 'avx512']:
    x=run([binary,'--no-aot',guest],{'ATTESOR_JIT_ON':flag},(flag or 'off')+'-neon-jit')
    assert x.stdout==expected, (flag,x.stdout,expected)
print('PASS neon_edge: interpreter == JIT off == JIT avx512 (runtime gating applies)',flush=True)
