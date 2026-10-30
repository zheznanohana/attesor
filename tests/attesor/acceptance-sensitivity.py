#!/usr/bin/env python3
"""Controlled CPU sensitivity proof. Run alone: temporarily mutates emit.rs,
requires each known-bad lowering to fail its oracle, then restores and rebuilds.
No production fault-injection switches; no git writes. Never run with another builder.
"""
import argparse,hashlib,json,os,subprocess,time
from pathlib import Path
root=Path(__file__).resolve().parents[2]
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--execute',action='store_true',required=True,help='allow temporary source mutations and local rebuilds')
parser.add_argument('--output',type=Path,required=True,help='new artifact directory')
args=parser.parse_args()
os.chdir(root)
out=args.output.resolve();out.mkdir(parents=True,exist_ok=False)
p=root/'crates/attesor/src/jit/emit.rs';good=p.read_text();rows=[]
(out/'emit.rs.original').write_text(good)
env={k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_')};env.update(TMPDIR='/private/tmp',PYTHONPYCACHEPREFIX=str(out/'pycache'))
def run(label,cmd,extra=None,bad=False):
 t=time.monotonic();r=subprocess.run(cmd,env={**env,**(extra or {})},capture_output=True,text=True,timeout=600)
 (out/(label+'.log')).write_text(r.stdout+r.stderr)
 passed=(r.returncode!=0 if bad else r.returncode==0)
 if bad:
  oracle = ('panicked at crates/attesor/src/jit/emit.rs:' in r.stderr and 'helper_unknown_writes_reconcile_and_conditional_fallthrough' in r.stderr and 'left == right' in r.stderr) if label=='bad-region-repair' else ('test result: FAILED' in r.stdout if cmd[0]=='cargo' else 'panicked at' in r.stderr and 'addrforms' in r.stderr)
  passed=passed and oracle
 rows.append(dict(label=label,command=cmd,env=extra or {},returncode=r.returncode,seconds=time.monotonic()-t,expected_failure=bad,passed=passed))
 (out/'summary.json').write_text(json.dumps(rows,indent=2)+'\n');print(label,r.returncode,passed,flush=True);assert passed,label
cargo=['cargo','test','--offline','--release','-p','attesor']
try:
 run('good-gates',cargo+['on_off_precedence_in_fresh_processes','--','--test-threads=1','--nocapture'])
 old='!jit_off("regioncache") && ';assert old in good;p.write_text(good.replace(old,'',1))
 run('bad-off-priority',cargo+['on_off_precedence_in_fresh_processes','--','--test-threads=1','--nocapture'],bad=True)
 old='let mult = if shift > 3 { 1 } else';assert old in good;p.write_text(good.replace(old,'let mult = if shift > 3 { 2 } else',1))
 run('bad-scale-build',['cargo','build','--offline','--release','-p','attesor'])
 run('bad-scale-loopra',['target/release/attesor','--selftest-jit','addrforms','4000','0x1234'],{'ATTESOR_JIT_ON':'loopra'},bad=True)
 run('bad-scale-off-control',['target/release/attesor','--selftest-jit','addrforms','4000','0x1234'],{'ATTESOR_JIT_ON':'loopra','ATTESOR_JIT_OFF':'loopra'})
 old='if n != 0xff && self.u.rc.reg[s] != n {';assert old in good;p.write_text(good.replace(old,'if false && n != 0xff && self.u.rc.reg[s] != n {',1))
 run('bad-region-repair',cargo+['helper_unknown_writes_reconcile_and_conditional_fallthrough','--','--test-threads=1','--nocapture'],bad=True)
finally:
 p.write_text(good)
 run('restored-workspace',['cargo','test','--offline','--release','--workspace','--','--test-threads=1'])
 run('restored-build',['cargo','build','--offline','--release','-p','attesor'])
 for on in ['loopra','regioncache','loopra,regioncache','']:
  run('restored-addr-'+(on.replace(',','-') or 'default'),['target/release/attesor','--selftest-jit','addrforms','100000','0x1234'],{'ATTESOR_JIT_ON':on})
 (out/'restored-sha256.txt').write_text(hashlib.sha256(p.read_bytes()).hexdigest()+'\n')
