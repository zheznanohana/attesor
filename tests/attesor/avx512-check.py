#!/usr/bin/env python3
"""Track B regression matrix. AVX-512 execution requires the OS feature detector;
passing on an unsupported host proves fallback only, not high-bank correctness.
"""
import argparse, json, os, re, subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('attesor', type=Path, nargs='?', default=Path('target/release/attesor'))
p.add_argument('--count', type=int, default=100000)
p.add_argument('--flags', nargs='+', default=['', 'avx512'], help='explicit ATTESOR_JIT_ON selections (empty string = off)')
p.add_argument('--classes', nargs='+', help='override the full selftest class list for L0')
p.add_argument('--directed-only', action='store_true', help='only neon_edge and pressure/AOT, no selftest or loopra matrix')
p.add_argument('--require-native', action='store_true', help='fail unless requested AVX-512 instructions actually appear (verified fixture/host only)')
p.add_argument('--output', type=Path, default=Path('target/avx512/check'))
a=p.parse_args(); root=a.output.resolve(); root.mkdir(parents=True,exist_ok=True)
binary=a.attesor.resolve()
env={k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_')}
env.update(TMPDIR='/private/tmp',ATTESOR_CACHE=str(root/'cache'))
classes=a.classes or 'blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks 0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000'.split()
rows=[]
def run(cmd, extra, name, timeout=900):
    x=subprocess.run(list(map(str,cmd)),env={**env,**extra},text=True,capture_output=True,timeout=timeout)
    (root/(name+'.log')).write_text(x.stdout+x.stderr)
    if x.returncode: raise RuntimeError(f'{name}: exit {x.returncode}; see log')
    return x
for flag in ([] if a.directed_only else a.flags):
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
for flag in a.flags:
    x=run([binary,'--no-aot',guest],{'ATTESOR_JIT_ON':flag},(flag or 'off')+'-neon-jit')
    assert x.stdout==expected, (flag,x.stdout,expected)
print(f'PASS neon_edge: interpreter == JIT for flags={a.flags} (runtime gating applies)',flush=True)
# A guest workload matching the unit pressure oracle, including disk-loaded
# high-bank code. Preserve ABI callee-saved D8-D15 around the generated body.
asm=['.text', '.globl _pressure', '.p2align 2', '_pressure:', 'sub sp,sp,#64']
asm += [f'stp d{n},d{n+1},[sp,#{(n-8)*8}]' for n in range(8,16,2)]
asm += [f'ldr q{n},[x1,#{n*16}]' for n in range(32)]
asm += [f'.inst {0x1e602800|(31<<16)|(n<<5)|n:#x}' for n in range(28)]
asm += [f'.inst {0x1e600800|(((n+1)%28)<<16)|(n<<5)|n:#x}' for n in range(28)]
asm += [f'str q{n},[x0,#{n*16}]' for n in range(32)]
asm += [f'ldp d{n},d{n+1},[sp,#{(n-8)*8}]' for n in range(8,16,2)]
asm += ['add sp,sp,#64','ret']
source=root/'pressure.s'; source.write_text('\n'.join(asm)+'\n')
pressure=root/'pressure'
run(['clang','-O2','-arch','arm64','tests/attesor/avx512-pressure.c',source,'-o',pressure],{},'pressure-build')
expected=run([binary,'--interp','--no-aot',pressure],{},'pressure-interp').stdout
for flag in a.flags:
    label=flag or 'off'; cache=root/('pressure-cache-'+label)
    cache.mkdir(exist_ok=True)
    extra={'ATTESOR_JIT_ON':flag,'ATTESOR_CACHE':str(cache),'ATTESOR_AOT_SYNC':'1',
           'ATTESOR_AOT_CHECKPOINT':'0','ATTESOR_STATS':'1'}
    if a.require_native:
        words=[0x1e602800|(31<<16)|(n<<5)|n for n in range(28)]
        words += [0x1e600800|(((n+1)%28)<<16)|(n<<5)|n for n in range(28)]
        words += [0xd4001001]
        dump=run([binary,'--jit-dump',*[f'{w:08x}' for w in words]],
                 {**extra,'ATTESOR_DUMP_BYTES':'65536'},label+'-pressure-dump').stdout
        data=bytes.fromhex(dump.strip())
        source=root/(label+'-pressure-host.s'); obj=source.with_suffix('.o')
        source.write_text('.text\n.globl _block\n_block:\n.byte '+','.join(map(str,data))+'\n')
        run(['clang','-arch','x86_64','-c',source,'-o',obj],{},label+'-pressure-host-build')
        dis=run(['objdump','-d','--no-show-raw-insn',obj],{},label+'-pressure-host-dis').stdout
        selected=flag.split(',')
        if 'avx512' in selected:
            assert re.search(r'%xmm(?:1[6-9]|2[0-9]|3[01])\b',dis), 'no high-bank code; runtime fallback is not native validation'
        if 'avx512nan' in selected:
            assert re.search(r'vfixupimm(?:ps|pd)',dis), 'no VFIXUPIMM code; runtime fallback is not native validation'
    cold=run([binary,pressure],extra,label+'-pressure-cold')
    assert cold.stdout==expected,(label,cold.stdout,expected)
    files=list(cache.glob('main-*.aot')); assert len(files)==1,files
    for f in cache.glob('*.aot'):
        if f!=files[0]: f.unlink()
    warm=run([binary,pressure],{**extra,'ATTESOR_NO_AOT_BUILD':'1'},label+'-pressure-warm')
    assert warm.stdout==expected,(label,warm.stdout,expected)
    assert re.search(r'aot: loaded [1-9]\d* blocks for pressure(?:\s|$)',warm.stderr),warm.stderr
    print(f'PASS pressure: interpreter=cold-JIT=warm-main-AOT flag={flag!r} {expected.strip()}',flush=True)
