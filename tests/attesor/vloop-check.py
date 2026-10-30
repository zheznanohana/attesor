#!/usr/bin/env python3
"""Real guest V-pressure checks, normal/disabled/low-bank JIT and private disk AOT.

No production install, user cache, app data, or network changes. Run serially,
separately from performance measurements. Outputs are correctness, not speed.
"""
import argparse,hashlib,json,os,pathlib,re,subprocess,tempfile
ROOT=pathlib.Path(__file__).resolve().parents[2]
def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('attesor',type=pathlib.Path);p.add_argument('--vchain',action='store_true');p.add_argument('--loopra2',action='store_true');p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
 binary=a.attesor.resolve(strict=True);output=a.output.resolve();output.parent.mkdir(parents=True,exist_ok=True)
 default=re.search(r'const DEFAULT_ON: &str = "(.*?)";',(ROOT/'crates/attesor/src/main.rs').read_text(),re.S)[1].replace('\\\n','')
 env={k:v for k,v in os.environ.items() if not k.startswith('ATTESOR_') and not k.startswith('DYLD_')}
 for k in ['ATTESOR_SYSROOT','ATTESOR_DYLD']:
  if k in os.environ:env[k]=os.environ[k]
 env.update(ATTESOR_JIT_ON=default+',vloop'+(',loopra2' if a.loopra2 else '')+(',vchain' if a.vchain else ''),ATTESOR_STATS='1',ATTESOR_AOT_CHECKPOINT='0')
 report=dict(status='running',translator=str(binary),translator_sha256=sha(binary),source_sha256=sha(ROOT/'tests/attesor/vloop-pressure.c'),samples=[])
 def save():output.write_text(json.dumps(report,indent=2)+'\n')
 def run(label,cmd,extra=None,expected=None):
  proc=subprocess.run([str(x) for x in cmd],env={**env,**(extra or {})},text=True,capture_output=True,timeout=300)
  row=dict(label=label,command=[str(x) for x in cmd],environment=extra or {},returncode=proc.returncode,stdout=proc.stdout,stderr=proc.stderr)
  report['samples'].append(row);save()
  if proc.returncode or (expected is not None and proc.stdout.strip()!=expected):raise RuntimeError(label+' failed: '+str(row))
  return proc
 try:
  with tempfile.TemporaryDirectory(prefix='attesor-vloop-') as tmp:
   d=pathlib.Path(tmp)
   for width in ['double','single']:
    flags=([] if width=='double' else ['-DSINGLE'])+(['-DVCHAIN'] if a.vchain else [])
    guest=d/(width+'-arm64');native=d/(width+'-x86_64')
    for arch,dest in [('arm64',guest),('x86_64',native)]:
     run('build-'+width+'-'+arch,['clang','-O2','-ffp-contract=off','-arch',arch,*flags,ROOT/'tests/attesor/vloop-pressure.c','-o',dest])
    expected=run('reference-'+width,[native]).stdout.strip()
    for mode,setting in [('on',{}),('off',{'ATTESOR_JIT_OFF':'vloop'}),('low-bank',{'ATTESOR_JIT_OFF':'avx512,avx512nan'})]+([('vchain-off',{'ATTESOR_JIT_OFF':'vchain'})] if a.vchain else []):
     cache=d/(width+'-'+mode+'-cache');cache.mkdir()
     local={**setting,'ATTESOR_CACHE':str(cache),'ATTESOR_NO_AOT_BUILD':'1'}
     run(width+'-'+mode+'-jit',[binary,'--no-aot',guest],local,expected)
     run(width+'-'+mode+'-prewarm',[binary,'--prewarm',guest],{**setting,'ATTESOR_CACHE':str(cache),'ATTESOR_AOT_SYNC':'1'})
     blobs={str(f):sha(f) for f in cache.glob('main-*.aot')}
     if not blobs:raise RuntimeError('prewarm produced no main artifact')
     for repeat in range(2):
      result=run(width+'-'+mode+'-warm-'+str(repeat),[binary,guest],local,expected)
      if not re.search(r'aot: loaded [1-9]\d* blocks for '+re.escape(guest.name)+r'(?:\s|$)',result.stderr):raise RuntimeError('actual main-image cache load not proven')
     if any(sha(pathlib.Path(f))!=digest for f,digest in blobs.items()):raise RuntimeError('warm run changed main artifact')
    run(width+'-interpreter',[binary,'--interp',guest],{'ATTESOR_CACHE':str(d/(width+'-interp')),'ATTESOR_NO_AOT_BUILD':'1'},expected)
   report['status']='passed';save();print('PASS: double/single, JIT on/off/low-bank'+('/vchain-off' if a.vchain else '')+', real main-image warm AOT, interpreter; no performance claim')
 except BaseException as e:report.update(status='failed',error=str(e));save();raise
if __name__=='__main__':main()
