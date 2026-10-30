#!/usr/bin/env python3
"""CPU acceptance ledger. All artifacts/caches are private; no git or app writes.

Run --phase perf alone (no other CPU work). --phase l1/l2/checks retain a
JSON ledger and raw logs. Nonzero exits and timeouts are never counted as PASS.
The inventory intentionally describes *this* source tree, not sibling worktrees.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import statistics
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
CLASSES = ('blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks addrforms jitcode offline '
           '0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000').split()
TOKENS = ('avx512 avx512nan bgtrans bliccache callret fastenc flagreg hugepages ibtc2 isel2 jitcost loopcensus '
          'loopra2 nativefixups notbi offline-constprop offline-dse offline-layout-profile page16k pathcache '
          'preciseinvalidate process-qos regioncache rel32chain sharedtrans slotreuse tiered loopra').split()
SEEDS = ['0x1234', '0xdeadbeef', '0xc0ffee', '0x100f5e1f7e57cafe']
FP = Path('target/bench/fp')
NODE = Path('/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness')
JS = Path.home() / 'attesor-dshprof/js/js.js'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--phase', required=True, choices=['l1', 'l2', 'perf', 'checks', 'headless'])
    p.add_argument('--attesor', type=Path, default=ROOT / 'target/release/attesor')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--runs', type=int, default=3)
    p.add_argument('--workloads', nargs='+', choices=['b','k','js','start'], default=['b','k','js','start'])
    p.add_argument('--tokens', nargs='+', choices=TOKENS, default=TOKENS)
    p.add_argument('--classes', nargs='+', choices=CLASSES, default=CLASSES)
    p.add_argument('--count', type=int, default=100000)
    p.add_argument('--loop-seeds', action='store_true', help='L1 extra 4 x 300k loop inputs in every configuration')
    p.add_argument('--skip-cargo', action='store_true')
    p.add_argument('--configs', nargs='+', help='optional exact L1 config labels')
    p.add_argument('--on', default='')
    p.add_argument('--off', default='')
    p.add_argument('--resume', action='store_true')
    p.add_argument('--base-off', default='', help='additional tokens disabled in both perf arms')
    a = p.parse_args()
    if a.runs < 3: p.error('--runs must be >=3')
    if a.count <= 0: p.error('--count must be positive')
    if not a.output.resolve().is_relative_to(ROOT/'target'): p.error('output must be under this worktree target/')
    if a.phase == 'l1' and a.output.name == 'l1':
        while not (ROOT/'target/acceptance/gates-ready').exists(): time.sleep(1)
    out = a.output.resolve(); out.mkdir(parents=True, exist_ok=a.resume)
    binary = a.attesor.resolve()
    sha = hashlib.sha256(binary.read_bytes()).hexdigest()
    env = {k:v for k,v in os.environ.items() if not k.startswith(('ATTESOR_', 'DYLD_'))}
    env.update(TMPDIR='/private/tmp', PYTHONPYCACHEPREFIX=str(out/'pycache'), ATTESOR_CACHE=str(out/'cache'),
               ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0',
               ATTESOR_JIT_ON=a.on, ATTESOR_JIT_OFF=a.off, ATTESOR_AUTOTUNE='0')
    manifest = out/'summary.json'
    data = json.loads(manifest.read_text()) if a.resume and manifest.exists() else dict(
        binary=str(binary), sha256=sha, phase=a.phase, started=time.ctime(), rows=[])
    assert data['sha256'] == sha, 'do not resume a ledger with a different binary'
    def save(): manifest.write_text(json.dumps(data, indent=2)+'\n')
    def run(label, command, extra=None, timeout=600, selftest=False):
        local = {**env, **(extra or {})}
        command = list(map(str, command))
        previous = next((r for r in reversed(data['rows']) if r['label']==label), None)
        if a.resume and previous and previous['status']=='passed' and previous['command']==command and previous['env']=={k:v for k,v in local.items() if k.startswith('ATTESOR_')}: return previous
        row = dict(label=label, command=command, env={k:v for k,v in local.items() if k.startswith('ATTESOR_')},
                   uptime=subprocess.run(['uptime'],capture_output=True,text=True).stdout.strip(), start=time.ctime())
        t=time.monotonic()
        try:
            child=subprocess.Popen(command,env=local,cwd=ROOT,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
            try:
                stdout,stderr=child.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid,signal.SIGKILL)
                stdout,stderr=child.communicate()
                raise subprocess.TimeoutExpired(command,timeout,output=stdout,stderr=stderr)
            r=subprocess.CompletedProcess(command,child.returncode,stdout,stderr)
            row.update(returncode=r.returncode, status='passed' if r.returncode==0 else 'failed', stdout=r.stdout, stderr=r.stderr)
        except subprocess.TimeoutExpired as e:
            row.update(returncode=None,status='timeout',stdout=(e.stdout or b'').decode(errors='replace') if isinstance(e.stdout,bytes) else e.stdout or '',stderr=(e.stderr or b'').decode(errors='replace') if isinstance(e.stderr,bytes) else e.stderr or '')
        row['seconds']=time.monotonic()-t
        (out/(label+'.log')).write_text(row['stdout']+row['stderr'])
        if selftest:
            match=re.search(r'^jit selftest .*$',row['stdout'],re.M)
            parsed=dict(re.findall(r'(\w+)=(0x[0-9a-fA-F]+|\d+)(?=\s|$)', match[0] if match else ''))
            if 'seed' in parsed: row['seed']=parsed.pop('seed')
            fields={k:int(v,16) if v.startswith('0x') else int(v) for k,v in parsed.items()}
            row['counters']=fields
            requested=int(command[command.index('--selftest-jit')+2])
            if not match or fields.get('mismatch')!=0 or fields.get('ok',0)<=0 or sum(fields.get(k,0) for k in ['ok','mismatch','skipped'])!=requested or 'MISMATCH' in row['stdout']:
                row['status']='failed'
        # Keep complete raw logs, not multi-megabyte diagnostic copies in JSON.
        for field in ('stdout', 'stderr'):
            if len(row[field]) > 16384:
                row[field] = row[field][:8192] + '\n[full output in raw log]\n' + row[field][-8192:]
        data['rows'].append(row); save()
        print(label,row['status'],round(row['seconds'],3), row.get('counters',''),flush=True)
        return row
    if a.phase in ('l1','l2'):
        if a.phase=='l1':
            configs={}
            all_on=','.join(a.tokens)
            for token in a.tokens:
                run('build-'+token,['cargo','build','--offline','--release','-p','attesor'],{'ATTESOR_JIT_ON':token})
                configs[token+'-off']=('',token)
                configs[token+'-on']=(token,'')
                configs[token+'-all']=(all_on,'')
                configs[token+'-off-wins']=(all_on,token)
            # Write-through code is otherwise shadowed by default-on loopra.
            if 'regioncache' in a.tokens:
                configs['regioncache-active']=('regioncache','loopra')
                configs['regioncache-active-off']=('regioncache','loopra,regioncache')
            if a.configs is not None and set(a.configs)-set(configs):
                p.error('unknown L1 config: '+','.join(sorted(set(a.configs)-set(configs))))
            # Identical combinations share evidence, explicitly identified.
            seen={}
            for name,opts in configs.items():
                if a.configs is not None and name not in a.configs: continue
                if opts in seen:
                    data.setdefault('aliases',{})[name]=seen[opts]; save(); continue
                seen[opts]=name
                extra=dict(ATTESOR_JIT_ON=opts[0],ATTESOR_JIT_OFF=opts[1])
                for cls in a.classes:
                    run(name+'-'+cls,[binary,'--selftest-jit',cls,a.count,SEEDS[0]],extra,selftest=True)
                for seed in (SEEDS if a.loop_seeds else []):
                    run(name+'-loops-'+seed,[binary,'--selftest-jit','loops',300000,seed],extra,selftest=True)
                for release in ([] if a.skip_cargo else [False,True]):
                    run(name+'-cargo'+('-release' if release else ''),['cargo','test','--offline',*(['--release'] if release else []),'--workspace','--','--test-threads=1'],extra)
        else:
            for seed in SEEDS:
                for cls in a.classes:
                    run(cls+'-'+seed,[binary,'--selftest-jit',cls,300000,seed],selftest=True)
            for release in ([] if a.skip_cargo else [False,True]):
                run('cargo'+('-release' if release else ''),['cargo','test','--offline',*(['--release'] if release else []),'--workspace','--','--test-threads=1'])
            run('aot-final-defaults',['bash',ROOT/'tests/attesor/loopra-aot-check.sh',binary],timeout=1800)
    elif a.phase=='perf':
        workloads={'b':[binary,'--no-aot',FP/'b-arm64'], 'k':[binary,'--no-aot',FP/'k-arm64'],
                   'js':[binary,'--no-aot',NODE,JS], 'start':[binary,'--no-aot',NODE,'-e','0']}
        workloads={k:v for k,v in workloads.items() if k in a.workloads}
        for prog in ['b','k']:
            if prog not in workloads: continue
            for i in range(a.runs): run(f'native-{prog}-{i}',[FP/(prog+'-x86')])
        native_js=None
        if 'js' in workloads:
            native_js=run('native-js-reference',[NODE,JS],{'ELECTRON_RUN_AS_NODE':'1'})
        for token in a.tokens:
            for i in range(a.runs):
                for mode in (['off','on'] if i%2==0 else ['on','off']):
                    extra=dict(ATTESOR_JIT_ON=token if mode=='on' else '',ATTESOR_JIT_OFF=','.join(x for x in [a.base_off,token if mode=='off' else ''] if x),ELECTRON_RUN_AS_NODE='1')
                    for work,cmd in workloads.items(): run(f'{token}-{mode}-{work}-{i}',cmd,extra,timeout=180)
        latest={r['label']:r for r in data['rows']}
        aggregates={}
        for token in a.tokens:
            results={}
            for work in workloads:
                samples={}
                for mode in ['off','on']:
                    rows=[r for r in latest.values() if r['label'].startswith(f'{token}-{mode}-{work}-')]
                    if any(r['status']!='passed' for r in rows): continue
                    values={'wall_ms':[r['seconds']*1000 for r in rows]}
                    for r in rows:
                        for name,val in re.findall(r'^\s*(\S+)\s+([\d.]+)\s*ms',r['stdout'],re.M):
                            values.setdefault(name,[]).append(float(val))
                    samples[mode]={k:dict(raw=v,median=statistics.median(v)) for k,v in values.items()}
                results[work]=samples
            aggregates[token]=results
        data['aggregates']=aggregates
        if native_js is not None:
            def js_values(text): return dict(re.findall(r'^(loops|objects|json|regex|strings|closures|sort)\s+\d+\s+ms\s+(.+)$',text,re.M))
            expected=js_values(native_js['stdout'])
            data['js_equal']=len(expected)==7 and all(js_values(r['stdout'])==expected for r in latest.values() if '-js-' in r['label'] and r['label']!='native-js-reference')
    elif a.phase=='headless':
        source=(Path.home()/'attesor-dshprof/net/t.js').read_text()
        # Keep every local crypto/Unicode line unmodified; omit only https.get.
        local=out/'crypto-local.js'; local.write_text(source[:source.index("https.get(")])
        native=run('native-crypto',[NODE,local],{'ELECTRON_RUN_AS_NODE':'1'})
        translated=run('translated-crypto',[binary,'--no-aot',NODE,local],{'ELECTRON_RUN_AS_NODE':'1'})
        expected=['sha256 ca6073392ee71dbd','sha512 fcf47d64f5ceadd9',
                  'gcm 89788ae8c19a5009 tag de047988105bcc38','ecdsa verify true','ecdh true',
                  'string 13 e5b7a6e4b88ae8a792e9a1b6e6a08f']
        data['crypto_reference']=expected
        data['crypto_equal']=native['status']==translated['status']=='passed' and native['stdout'].splitlines()==translated['stdout'].splitlines()==expected
        run('node-zero',[binary,'--no-aot',NODE,'-e','0'],{'ELECTRON_RUN_AS_NODE':'1'})
        native_js=run('native-js',[NODE,JS],{'ELECTRON_RUN_AS_NODE':'1'})
        translated_js=run('translated-js',[binary,'--no-aot',NODE,JS],{'ELECTRON_RUN_AS_NODE':'1'})
        def values(text): return dict(re.findall(r'^(loops|objects|json|regex|strings|closures|sort)\s+\d+\s+ms\s+(.+)$',text,re.M))
        data['js_equal']=len(values(native_js['stdout']))==7 and values(native_js['stdout'])==values(translated_js['stdout'])
    elif a.phase=='checks':
        # Curated CPU-only entry points: never guess CLIs or recursively run
        # integration/mutation/performance runners discovered through a glob.
        scripts = [
            ('autotune', ['python3', ROOT/'tests/attesor/autotune-check.py']),
            ('regioncache', ['python3', ROOT/'tests/attesor/regioncache-check.py', binary]),
            ('weighted-layout', ['python3', ROOT/'tests/attesor/weighted-layout-check.py']),
            ('offline', ['python3', ROOT/'tests/attesor/offline-check.py', '--attesor', binary,
                         '--jit-on', 'offline-constprop,loopra2,isel2,fastenc,slotreuse']),
            ('page16k', ['python3', ROOT/'tests/attesor/page16k-check.py']),
            ('process-qos', ['python3', ROOT/'tests/attesor/process-qos-check.py']),
            ('nativefixups', ['python3', ROOT/'tests/attesor/nativefixups-check.py', '--output', out/'nativefixups']),
            ('profile-report', ['python3', ROOT/'tests/attesor/profile-report-check.py']),
            ('icache-handoff', ['python3', ROOT/'tests/attesor/icache-handoff-check.py', binary]),
            ('fp16-status', ['python3', ROOT/'tests/attesor/fp16-status-check.py', binary]),
            ('gdb-protocol', ['python3', ROOT/'tests/attesor/gdb-protocol.py', binary]),
            ('gdb-lldb-fd', ['python3', ROOT/'tests/attesor/gdb-lldb-fd.py', binary]),
            ('crypto', ['bash', ROOT/'tests/attesor/crypto-check.sh']),
        ]
        for label, cmd in scripts:
            run(label, cmd, timeout=1800)
    data['finished']=time.ctime()
    latest={r['label']:r for r in data['rows']}
    data['latest_commands']=len(latest)
    data['failed_attempts']=sum(r['status']!='passed' for r in data['rows'])
    data['passed']=all(r['status']=='passed' for r in latest.values()) and data.get('crypto_equal',True) and data.get('js_equal',True) and not data.get('blocked')
    save()
    print('ledger:',manifest,'passed:',data['passed'],flush=True)
    return 0 if data['passed'] else 1

if __name__=='__main__': raise SystemExit(main())
