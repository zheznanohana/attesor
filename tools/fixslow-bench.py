#!/usr/bin/env python3
"""CPU-only fixslow A/B: private caches, interleaved binaries, raw phase times.

--binary label=path can be repeated (old/new/candidate). No user's cache, GUI,
network, Instruments or external profiler. QoS response is a launch-to-exit Node
latency proxy, not UI responsiveness. All own builders are joined before exit.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import signal
import shutil
import subprocess
import threading
import time

REPO = Path(__file__).resolve().parents[1]
ELECTRON = '/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness'


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary', action='append', required=True, help='label=path')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--pairs', type=int, default=3)
    p.add_argument('--workloads', nargs='+', choices=['fixups', 'fixups-guest-first', 'fixups-cold', 'launch', 'prewarm', 'contention', 'response'],
                   default=['fixups', 'prewarm', 'contention', 'response'])
    p.add_argument('--builder-jobs', type=int, default=12)
    p.add_argument('--default-off', action='store_true', help='OFF is unset tokens, rather than explicit OFF token')
    args = p.parse_args()
    if args.pairs < 3: p.error('at least 3 pairs')
    root = args.output.resolve()
    if not (root.is_relative_to(REPO) or root.is_relative_to(Path('/private/tmp'))): p.error('private output required')
    root.mkdir(parents=True, exist_ok=False)
    binaries = [(name, Path(path).resolve()) for name, path in (b.split('=', 1) for b in args.binary)]
    (root/'manifest.json').write_text(json.dumps(dict(binaries={n:dict(path=str(b), sha256=hashlib.sha256(b.read_bytes()).hexdigest()) for n,b in binaries},
        argv=os.sys.argv, uname=list(os.uname()), sources={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in [REPO/'crates/attesor/src/nativefixups.rs',REPO/'crates/attesor/src/process_qos.rs']}), indent=2)+'\n')
    env = {k:v for k,v in os.environ.items() if not k.startswith(('ATTESOR_', 'DYLD_'))}
    env.update(TMPDIR='/private/tmp', ELECTRON_RUN_AS_NODE='1', ATTESOR_AOT_CHECKPOINT='0',
               ATTESOR_NO_AOT_BUILD='1', ATTESOR_STATS='1', ATTESOR_CACHE=str(root/'guest-cache'))
    src = root/'trivial.c'; src.write_text('int main(void) { return 0; }\n')
    guest = root/'trivial'
    subprocess.run(['clang', '-O2', '-arch', 'arm64', str(src), '-o', str(guest)], env=env, check=True)
    subprocess.run([str(binaries[0][1]), '--no-aot', str(guest)], env=env, stdout=subprocess.DEVNULL, stderr=(root/'warmup.stderr').open('w'), check=True, timeout=180)
    node = [ELECTRON, '-e', 'console.log("PASS Node", require("crypto").createHash("sha256").update("nativefixups").digest("hex"))']
    response = [ELECTRON, '-e', 'let x=0;for(let i=0;i<1000000;i++)x=(x+i)|0;console.log("PASS JS",x)']
    def invoke(binary, command, e, label):
        start = time.perf_counter()
        with (root/(label+'.stdout')).open('w') as out, (root/(label+'.stderr')).open('w') as err:
            child = subprocess.Popen([str(binary), *map(str,command)], env=e, stdout=out, stderr=err, start_new_session=True)
            def timeout():
                try: os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError: pass
            watchdog = threading.Timer(180, timeout)
            watchdog.start()
            try: rc = child.wait()  # blocking: no timeout polling/50ms quantization
            finally:
                watchdog.cancel()
                watchdog.join()
        end = time.perf_counter()
        if rc: raise RuntimeError(f'{label}: rc={rc}')
        return dict(start=start, end=end, wall_s=end-start)
    with (root/'measurements.jsonl').open('w') as f:
        for pair in range(args.pairs):
            for workload in args.workloads:
                for name, binary in binaries:
                    modes = ['guest','on'] if workload=='fixups-cold' else ['verify'] if workload.startswith('fixups') else ['off','on']
                    # Counterbalance time trends while keeping adjacent pairs.
                    if pair % 2: modes = list(reversed(modes))
                    for mode in modes:
                        label = f'{pair}-{workload}-{name}-{mode}'
                        feature = 'nativefixups' if (workload.startswith('fixups') or workload=='launch') else 'process-qos'
                        e = {**env, 'ATTESOR_JIT_OFF': feature} if mode=='off' else {**env, 'ATTESOR_JIT_ON':feature}
                        if mode=='off' and args.default_off: e = dict(env)
                        if mode=='guest': e['ATTESOR_NATIVEFIXUPS_GUEST_TIMING']='1'
                        if mode=='verify': e['ATTESOR_NATIVEFIXUPS_VERIFY']='guest-first' if workload=='fixups-guest-first' else '1'
                        row = dict(pair=pair, workload=workload, binary=name, mode=mode)
                        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
                        if workload in ('contention','response'):
                            jobs = 1 if workload=='contention' else args.builder_jobs
                            bg, errors = [], []
                            ready = threading.Event()
                            def worker():
                                try:
                                    for j in range(jobs):
                                        cache = root/(label+f'-builder-{j}')
                                        be = {**e, 'ATTESOR_CACHE': str(cache)}
                                        # Mark just before spawning the first dedicated builder.
                                        ready.set()
                                        bg.append(invoke(binary, ['--prewarm',guest], be, label+f'-builder-{j}'))
                                        shutil.rmtree(cache)
                                except BaseException as exc: errors.append(exc); ready.set()
                            thread = threading.Thread(target=worker)
                            thread.start(); ready.wait()
                            try:
                                fg = invoke(binary, ['--no-aot',*(response if workload=='response' else [ELECTRON,'-e','0'])],e,label)
                            finally: thread.join()
                            if errors: raise errors[0]
                            assert any(min(fg['end'],x['end']) > max(fg['start'],x['start']) for x in bg), 'builder must actually overlap foreground'
                            row.update(foreground_s=fg['wall_s'], builder_s=sum(x['wall_s'] for x in bg),
                                       overlap_s=sum(max(0,min(fg['end'],x['end'])-max(fg['start'],x['start'])) for x in bg),
                                       wall_s=max(fg['end'],bg[-1]['end'])-min(fg['start'],bg[0]['start']), builder_jobs=jobs)
                        elif workload=='prewarm':
                            cache=root/(label+'-cache')
                            row.update(invoke(binary,['--prewarm',guest],{**e,'ATTESOR_CACHE':str(cache)},label))
                            shutil.rmtree(cache)
                        else: row.update(invoke(binary,['--no-aot',*node],e,label))
                        after=resource.getrusage(resource.RUSAGE_CHILDREN)
                        row.update(cpu_s=after.ru_utime+after.ru_stime-usage.ru_utime-usage.ru_stime,
                                   uptime=subprocess.check_output(['uptime'],text=True).strip())
                        stderr=(root/(label+'.stderr')).read_text()
                        if workload in ('launch','fixups','fixups-guest-first','fixups-cold'):
                            assert 'PASS Node' in (root/(label+'.stdout')).read_text(), label
                        if workload=='response': assert 'PASS JS 1783293664' in (root/(label+'.stdout')).read_text(), label
                        row['fixups']=[dict(mode=m,image=i,pages=int(p),rebases=int(r),binds=int(b),native_ns=int(n),guest_ns=int(g)) for m,i,p,r,b,n,g in re.findall(r'mode=([\w-]+) image=(0x[\da-f]+) pages=(\d+) rebases=(\d+) binds=(\d+) native_ns=(\d+) guest_ns=(\d+)',stderr)]
                        row['phases']=[dict(plan_ns=int(a),vm_ns=int(b),write_ns=int(c)) for a,b,c in re.findall(r'phases: plan_ns=(\d+) vm_ns=(\d+) write_ns=(\d+)',stderr)]
                        f.write(json.dumps(row)+'\n'); f.flush()
                        print(json.dumps(row),flush=True)
    # Large, reproducible private DSC data is disposable; keep logs + JSON.
    shutil.rmtree(root/'guest-cache')


if __name__=='__main__': main()
