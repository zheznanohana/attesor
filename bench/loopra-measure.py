#!/usr/bin/env python3
"""Three alternating loopra on/off runs of the repository's unchanged CPU benchmarks."""
import argparse,json,os,subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('attesor',type=Path)
p.add_argument('output',type=Path)
p.add_argument('--dir',type=Path,default=Path('target/loopra-aot'))
a=p.parse_args(); a.dir.mkdir(parents=True,exist_ok=True)
env={k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_')}
env.update(TMPDIR='/private/tmp',ATTESOR_CACHE=str((a.dir/'unused-perf-cache').resolve()))
for name,src in [('bench','bench/bench.c'),('kernels','bench/native-ratio/kernels.c')]:
 if not (a.dir/name).exists(): subprocess.run(['clang','-O2','-arch','arm64',src,'-o',str(a.dir/name)],env=env,check=True)
rows=[]
for i in range(3):
 for off in ('','loopra'):
  row={'run':i+1,'off':off,'load':subprocess.check_output(['uptime'],text=True).strip()}
  for bench in ('bench','kernels'):
   row[bench+'_load']=subprocess.check_output(['uptime'],text=True).strip()
   x=subprocess.run([str(a.attesor.resolve()),'--no-aot',str((a.dir/bench).resolve())],env={**env,'ATTESOR_JIT_OFF':off},text=True,capture_output=True,check=True)
   row[bench]=x.stdout
  rows.append(row); print(json.dumps(row),flush=True)
  a.output.write_text(json.dumps(rows,indent=2)+'\n')
