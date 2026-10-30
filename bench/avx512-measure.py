#!/usr/bin/env python3
"""Track B timings and static hot-block instruction counts; private caches only.
Run baseline before editing, then A/B: --mode baseline | compare.
Counts follow the default-FPCR entry guard and stop at the first unconditional
host transfer (cold NaN tails and dispatcher stubs are excluded, not dynamic counts).
"""
import argparse, json, os, re, subprocess
from pathlib import Path
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('attesor', type=Path)
p.add_argument('output', type=Path)
p.add_argument('--mode', choices=['baseline', 'compare'], default='baseline')
p.add_argument('--runs', type=int, default=3)
p.add_argument('--flags', nargs='+', help='explicit ATTESOR_JIT_ON variants; empty string = off')
a = p.parse_args()
variants = a.flags if a.flags is not None else ([''] if a.mode == 'baseline' else ['', 'avx512'])
root = a.output.resolve(); root.mkdir(parents=True, exist_ok=True)
env = {k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_')}
env.update(TMPDIR='/private/tmp', ATTESOR_CACHE=str(root/'cache'))
def run(cmd, extra=None):
    return subprocess.check_output(list(map(str,cmd)), env={**env, **(extra or {})}, text=True, stderr=subprocess.PIPE, timeout=300)
binary = a.attesor.resolve(); guest = root/'kernels'
run(['clang','-O2','-arch','arm64','bench/avx512-kernels.c','-o',guest])
text = run(['otool','-tv',guest]); (root/'kernels.otool').write_text(text)
raw = {}
for line in run(['otool','-t',guest]).splitlines():
    m=re.match(r'^([0-9a-f]{16})\s+((?:[0-9a-f]{8}\s*)+)$',line)
    if m:
        addr=int(m[1],16)
        for i,w in enumerate(m[2].split()): raw[addr+4*i]=w
functions={}; name=None
for line in text.splitlines():
    if line in ['_nbody:', '_matmul:', '_neon_dot:']:
        name=line[1:-1]; functions[name]=[]
    elif line.endswith(':'): name=None
    elif name and re.match(r'^[0-9a-f]{16}\s',line):
        fields=line.split(None,2); functions[name].append((int(fields[0],16), fields[1], fields[2] if len(fields)>2 else ''))
# Choose the actual binary's arithmetic body and two innermost backwards loops.
blocks={}
f=functions['nbody']; start=next(i for i,x in enumerate(f) if x[1]=='fsub')-3
end=next(i for i in range(start,len(f)) if f[i][1]=='b')
blocks['fp-nbody']=f[start:end+1]
for func,label,needle in [('matmul','matmul','fmadd'),('neon_dot','neon-dot','umlal.4s')]:
    f=functions[func]; op=next(i for i,x in enumerate(f) if x[1]==needle)
    end=next(i for i in range(op,len(f)) if f[i][1]=='b.ne')
    target=int(f[end][2].split()[0],16)
    start=next(i for i,x in enumerate(f) if x[0]==target)
    blocks[label]=f[start:end+1]
(root/'guest-blocks.json').write_text(json.dumps({k:[raw[x[0]] for x in v] for k,v in blocks.items()},indent=2)+'\n')
counts=[]
for flag in variants:
    for label,block in blocks.items():
        stem=label+'-'+(flag.replace(',','-') or 'off')
        dump=run([binary,'--jit-dump',*[raw[x[0]] for x in block]], {'ATTESOR_JIT_ON':flag,'ATTESOR_DUMP_BYTES':'16384'})
        (root/(stem+'.hex')).write_text(dump)
        data=bytes.fromhex(dump.strip())
        asm=root/(stem+'.s'); obj=root/(stem+'.o')
        asm.write_text('.text\n.globl _block\n_block:\n.byte '+','.join(str(x) for x in data)+'\n')
        run(['clang','-arch','x86_64','-c',asm,'-o',obj])
        dis=run(['objdump','-d','--no-show-raw-insn',obj]); (root/(stem+'.dis')).write_text(dis)
        ins=[]
        for line in dis.splitlines():
            m=re.match(r'^\s*([0-9a-f]+):\s+(\S+)\s*(.*)',line)
            if m: ins.append((int(m[1],16),m[2],m[3]))
        hot=[]; i=0
        while i<len(ins):
            inst=ins[i]; hot.append(inst)
            # Only the FPCR guard has the interpreter-return jump right after JE.
            if inst[1] in ('je','jz') and i>0 and 'test' in ins[i-1][1] and 'r15' in ins[i-1][2] and i+3<len(ins) and any(x[1].startswith('jmp') for x in ins[i+1:i+5]):
                target=int(inst[2].split()[0],16); i=next(j for j,x in enumerate(ins) if x[0]==target); continue
            if inst[1].startswith('jmp'): break
            i+=1
        assert hot and hot[-1][1].startswith('jmp'), (label, 'missing hot-block transfer')
        counts.append({'flag':flag,'kernel':label,'guest_insns':len(block),'hot_host_insns':len(hot), 'hot':hot})
(root/'counts.json').write_text(json.dumps(counts,indent=2)+'\n')
rows=[]
for i in range(a.runs):
    for flag in variants:
        row={'run':i+1,'flag':flag,'uptime':run(['uptime']).strip(),'stdout':run([binary,'--no-aot',guest],{'ATTESOR_JIT_ON':flag})}
        rows.append(row); print(json.dumps(row),flush=True)
        (root/'timings.json').write_text(json.dumps(rows,indent=2)+'\n')
print(json.dumps([{k:v for k,v in x.items() if k!='hot'} for x in counts],indent=2))
