#!/usr/bin/env python3
"""Serial, fresh-process L0 and source-restored sensitivity gates; no Git writes."""
import os,re,subprocess,argparse
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
OUT=ROOT/'target/port-g4'
TOKENS={'31':'hugepages','5':'flagreg','7':'isel2','33':'page16k','28':'sharedtrans'}
def run(name,cmd,on='',off='',fail=False,extra=None):
 env={k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_')}
 env.update(ATTESOR_JIT_ON=on,ATTESOR_JIT_OFF=off,ATTESOR_CACHE=str(OUT/'cache'))
 env.update(extra or {})
 p=OUT/'logs'/f'{name}.log';p.parent.mkdir(parents=True,exist_ok=True)
 with p.open('w') as f:r=subprocess.run(cmd,cwd=ROOT,env=env,stdout=f,stderr=subprocess.STDOUT,timeout=300)
 s=p.read_text()
 if fail: assert r.returncode!=0 and 'test result: FAILED' in s,(name,r.returncode,s[-3000:])
 else:
  assert r.returncode==0,(name,r.returncode,s[-5000:])
  if 'test' in cmd: assert re.search(r'test result: ok\. [1-9]\d* passed',s),(name,s[-1000:])
  if '--selftest-jit' in cmd: assert re.search(r'mismatch=0\b',s) and int(re.search(r'\bok=(\d+)',s).group(1))>0,(name,s[-1000:])
 print(('SENSITIVE ' if fail else 'PASS ')+name,flush=True)
 return s
def test(name,filt,on='',off='',fail=False,extra=None):
 return run(name,['cargo','test','--release','-p','attesor',filt,'--','--test-threads=1','--nocapture'],on,off,fail,extra)
MUTATIONS={
 '28':('crates/attesor/src/jit/sharedtrans.rs','read_exact(unit.pc, &mut current) && current == unit.source','true /* mutant: trust stale producer source */'),
 '33':('crates/attesor/src/syscalls.rs','args[1] = (end & !MASK) - start;','args[1] = len; // mutant: leave host tail unnormalized'),
 '7':('crates/attesor/src/jit/emit.rs','0 => a!(self, shlx(d, n, m)),','0 => a!(self, shlx(d, m, n)), // mutant: swap value/count; same mnemonic'),
 '5':('crates/attesor/src/jit/emit.rs','self.flags_sub(); // ARM C = !borrow; N/Z/V use the existing path','/* mutant: omit public NZCV repair */'),
 '31':('crates/attesor/src/jit/code_cache_alloc.rs','(self.release)(self.task, self.base, self.len)','0 /* mutant: leak rejected mapping */'),
}
def main():
 p=argparse.ArgumentParser();p.add_argument('feature',choices=[*TOKENS,'all']);p.add_argument('--sensitivity',action='store_true');p.add_argument('--final',action='store_true');a=p.parse_args()
 if a.final: final_gates();return
 for n in (TOKENS if a.feature=='all' else [a.feature]):
  t=TOKENS[n];f='g4_'+t
  run(n+'-build',['cargo','build','--release','-p','attesor'])
  for label,on,off in [('default','',''),('on',t,''),('off',t,t),('exact','x'+t+', '+t,'')]:test(n+'-'+label,f,on,off)
  # Never wire 4 GiB solely for L0: hugepages ON was tested using a tiny fallback mapping.
  for cls in {'31':['blocks'],'5':['blocks','loops'],'7':['dpreg','blocks','loops'],'33':[],'28':['blocks','loops']}[n]:
   run(n+'-selftest-'+cls,[str(ROOT/'target/release/attesor'),'--selftest-jit',cls,'1000','0x1234'],'' if n=='31' else t)
  if a.sensitivity:
   path,old,new=MUTATIONS[n];path=ROOT/path;src=path.read_text();assert src.count(old)==1,(path,old)
   try:
    path.write_text(src.replace(old,new));test(n+'-mutant',f,t,fail=True)
   finally:path.write_text(src)
   test(n+'-restored',f,t)
def final_gates():
 # Same-mnemonic semantic sensitivity (not merely an opcode-shape failure).
 path,old,new=MUTATIONS['7'];path=ROOT/path;src=path.read_text();assert src.count(old)==1
 try:
  path.write_text(src.replace(old,new));test('7-semantic-mutant','g4_isel2','isel2',fail=True)
 finally:path.write_text(src)
 test('7-semantic-restored','g4_isel2','isel2')
 run('final-build',['cargo','build','--release','-p','attesor'])
 run('final-workspace-L0',['cargo','test','--release','--workspace','--','--test-threads=1'])
 new='flagreg,isel2,page16k,sharedtrans'
 test('final-new-combined-L0','',new)
 wave=new+',tiered,bgtrans,fastenc,preciseinvalidate,slotreuse,jitcost,bliccache,ibtc2,rel32chain,notbi'
 test('final-wave1-combined-L0','',wave,extra={'ATTESOR_JIT_TIER_THRESHOLD':'3'})
 test('final-new-off','g4_',new,new)
 test('final-directbranch-off','g4_flagreg','flagreg','directbranch')
 test('final-shared-precise','g4_sharedtrans','sharedtrans,preciseinvalidate,rel32chain,fastenc')
 test('final-shared-jitcost-fallback','g4_sharedtrans','sharedtrans,jitcost')
 for name in ['ATTESOR_JIT_VERIFY','ATTESOR_STATS','ATTESOR_PROFILE']:
  test('final-shared-policy-'+name,'g4_sharedtrans_policy','sharedtrans',extra={name:'1'})
 for cls in ['dpreg','blocks','fpblocks','loops','calls','jitcode','offline']:
  run('final-selftest-'+cls,[str(ROOT/'target/release/attesor'),'--selftest-jit',cls,'1000','0x1234'],new)
 ids={}
 all_tokens=','.join(TOKENS.values())
 for label,on,off in [('default','',''),*[(t,t,'') for t in TOKENS.values()],('all',all_tokens,''),('off',all_tokens,all_tokens)]:
  s=test('final-identity-'+label,'g4_cache_identity',on,off)
  ids[label]=re.search(r'G4_ID=([0-9a-f]+)',s).group(1)
 assert len(set(ids.values()))==len(ids),ids
 print('PASS codegen/AOT identities:',ids)

if __name__=='__main__':main()

