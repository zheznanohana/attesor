"""Sequential CPU-only L1 matrix. Run after, never concurrently with benchmarks."""
import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    repo=Path(__file__).resolve().parents[1]
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor',type=Path,default=repo/'target/release/attesor')
    p.add_argument('--output',type=Path,required=True)
    args=p.parse_args()
    binary=args.attesor.resolve()
    root=args.output.resolve()
    if not (root.is_relative_to(repo) or root.is_relative_to(Path('/private/tmp'))):
        p.error('private worktree or /private/tmp output required')
    if not binary.is_file(): p.error('attesor binary not found')
    root.mkdir(parents=True,exist_ok=False)
    classes='blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks'.split()
    base={k:v for k,v in os.environ.items() if not k.startswith(('ATTESOR_','DYLD_'))}
    base.update(TMPDIR='/private/tmp',ATTESOR_NO_AOT_BUILD='1',ATTESOR_AOT_CHECKPOINT='0')
    with tempfile.TemporaryDirectory(prefix='fixslow-qa-',dir='/private/tmp') as td:
        base['ATTESOR_CACHE']=td
        for label,on in [('off',''),('native','nativefixups'),('qos','process-qos'),('combined','nativefixups,process-qos')]:
            env={**base,'ATTESOR_JIT_ON':on}
            if label=='off':env['ATTESOR_JIT_OFF']='nativefixups,process-qos'
            with (root/f'cargo-{label}.log').open('w') as log:
                subprocess.run(['cargo','test','--release','--workspace','--','--test-threads=1'],cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=300)
            with (root/f'selftest-{label}.log').open('w') as log:
                for cls in classes:
                    subprocess.run([binary,'--selftest-jit',cls,'100000'],env=env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=300)
                for seed in ['1','0xdeadbeef','0x100f5e1f7e57cafe']:
                    subprocess.run([binary,'--selftest-jit','loops','20000',seed],env=env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=300)
            print('PASS',label,'cargo and all 13 classes, loops 3 seeds',flush=True)
            # loopra's test deliberately cleans ATTESOR_* from its environment.
            # Wrap its binary so each child still receives the selected tokens.
            wrapper=root/f'attesor-{label}-wrapper'
            flags=f'ATTESOR_JIT_ON={on}' if on else 'ATTESOR_JIT_OFF="${ATTESOR_JIT_OFF:+$ATTESOR_JIT_OFF,}nativefixups,process-qos"'
            wrapper.write_text(f'#!/bin/sh\nexport {flags}\nexec "{binary}" "$@"\n')
            wrapper.chmod(0o755)
            with (root/f'loopra-{label}.log').open('w') as log:
                subprocess.run(['bash','tests/attesor/loopra-aot-check.sh',wrapper],cwd=repo,env=env,stdout=log,stderr=subprocess.STDOUT,check=True,timeout=600)
            print('PASS',label,'7 native/interpreter/JIT/AOT workloads',flush=True)
        for test in ['gdb-protocol.py','gdb-lldb-fd.py']:
            with (root/test.replace('.py','.log')).open('w') as log:
                subprocess.run(['python3',f'tests/attesor/{test}',binary],cwd=repo,env={**base,'ATTESOR_JIT_ON':'nativefixups,process-qos'},stdout=log,stderr=subprocess.STDOUT,check=True,timeout=120)
            print('PASS',test,flush=True)
        with (root/'js-combined.log').open('w') as log:
            subprocess.run([binary,'--no-aot','/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness',repo/'docs/tasks/phase2/js.js'],env={**base,'ATTESOR_JIT_ON':'nativefixups,process-qos','ELECTRON_RUN_AS_NODE':'1'},stdout=log,stderr=subprocess.STDOUT,check=True,timeout=180)
        print('PASS combined JS',flush=True)


if __name__=='__main__': main()
