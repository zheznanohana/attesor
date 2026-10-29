#!/usr/bin/env python3
"""Headless #30: full fixed-page shadow comparison, actual native path and A/B.

Everything written is under --output (worktree or /private/tmp). Never runs a GUI
or uses a user cache. --pairs defaults to 3 alternating OFF/ON measurements;
verification timing is NOT used as the fast-path launch time.
"""
import argparse
import json
import os
from pathlib import Path
import re
import resource
import struct
import subprocess
import time

REPO = Path(__file__).resolve().parents[2]
ELECTRON = Path('/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--attesor', type=Path, default=REPO/'target/release/attesor')
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--pairs', type=int, default=3)
    args = p.parse_args()
    root = args.output.resolve()
    if not (root.is_relative_to(REPO) or root.is_relative_to(Path('/private/tmp'))):
        p.error('output must be private to this worktree or /private/tmp')
    if args.pairs < 3:
        p.error('at least three pairs required')
    root.mkdir(parents=True, exist_ok=False)
    env = {k: v for k, v in os.environ.items() if not k.startswith('ATTESOR_') and not k.startswith('DYLD_')}
    env.update(TMPDIR='/private/tmp', ELECTRON_RUN_AS_NODE='1', ATTESOR_CACHE=str(root/'cache'),
               ATTESOR_NO_AOT_BUILD='1', ATTESOR_AOT_CHECKPOINT='0', ATTESOR_STATS='1')
    binary = args.attesor.resolve()
    def build(name, source, flags):
        src = root/name
        src.write_text(source)
        dest = src.with_suffix('' if src.suffix == '.m' else '.bin')
        subprocess.run(['clang', '-arch', 'arm64', '-O2', str(src), *flags, '-o', str(dest)], env=env, check=True)
        return dest
    trivial = build('trivial.c', 'int main(void) { return 0; }\n', [])
    foundation = build('foundation.m', '''#import <Foundation/Foundation.h>
#include <stdio.h>
int main(void) { @autoreleasepool {
 NSArray *a = @[@"beta", @"alpha", @"gamma"];
 NSString *s = [[a sortedArrayUsingSelector:@selector(compare:)] componentsJoinedByString:@","];
 NSData *d = [NSJSONSerialization dataWithJSONObject:@{@"count":@3} options:0 error:NULL];
 if (![s isEqualToString:@"alpha,beta,gamma"] || !d.length) return 2;
 puts("PASS Foundation: alpha,beta,gamma count=3");
} return 0; }
''', ['-fobjc-arc', '-framework', 'Foundation', '-Wl,-fixup_chains'])
    # A large standalone library with both rebases and dyld-resolved binds,
    # plus a weak import and embedded positive addends. No host loading of ARM.
    libsrc = root/'big.c'
    entries = ',\n'.join(['&local', '(char *)&puts+255', '(char *)&local+1', '(void *)&absent']*32768)
    libsrc.write_text('''#include <stdio.h>
static int local;
extern void absent(void) __attribute__((weak_import));
static void *volatile ptrs[] = {'''+entries+'''};
int check(void) {
 for (int i=0;i<131072;i+=4) {
  if (ptrs[i]!=&local || ptrs[i+1]!=(char*)&puts+255 || ptrs[i+2]!=(char*)&local+1 || ptrs[i+3]!=(void*)&absent) return 1;
 }
 return 0;
}
''')
    lib = root/'big.dylib'
    subprocess.run(['clang', '-arch', 'arm64', '-O2', '-dynamiclib', str(libsrc), '-Wl,-fixup_chains', '-Wl,-U,_absent', '-o', str(lib)], env=env, check=True)
    big = build('big-loader.c', '''#include <dlfcn.h>
#include <stdio.h>
int main(int argc,char **argv) {
 for(int i=0;i<2;i++) {
  void *h=dlopen(argv[1],RTLD_NOW|RTLD_LOCAL); if(!h) { puts(dlerror()); return 2; }
  int (*f)(void)=dlsym(h,"check"); if(!f || f()) return 3;
  if(dlclose(h)) return 4;
 }
 puts("PASS big dylib: 131072 pointers, weak bind, addend, reopen"); return 0;
}
''', ['-Wl,-fixup_chains'])
    workloads = {'trivial': ([trivial], ''), 'node': ([ELECTRON, '-e', 'console.log("PASS Node",require("crypto").createHash("sha256").update("nativefixups").digest("hex"))'], 'PASS Node'),
                 'foundation': ([foundation], 'PASS Foundation'), 'big': ([big, lib], 'PASS big dylib')}
    rows = []
    def run(name, variant, command, extra=None, opts=('--no-aot',)):
        dest = root/name
        e = dict(env)
        if variant != 'off': e['ATTESOR_JIT_ON'] = 'nativefixups'
        if variant == 'verify': e['ATTESOR_NATIVEFIXUPS_VERIFY'] = '1'
        if variant == 'precedence': e['ATTESOR_JIT_OFF'] = 'nativefixups'
        if extra: e.update(extra)
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.monotonic()
        r = subprocess.run([str(binary), *opts, *map(str, command)], env=e, capture_output=True, timeout=180)
        elapsed = time.monotonic()-start
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        dest.with_suffix('.stdout').write_bytes(r.stdout)
        dest.with_suffix('.stderr').write_bytes(r.stderr)
        row = dict(name=name, variant=variant, wall_s=elapsed,
                   cpu_s=after.ru_utime+after.ru_stime-before.ru_utime-before.ru_stime,
                   returncode=r.returncode, uptime=subprocess.check_output(['uptime'], text=True).strip())
        text = r.stderr.decode(errors='replace')
        metrics = [dict(mode=m, image=image, pages=int(p), rebases=int(r), binds=int(b), native_ns=int(n), guest_ns=int(g))
                   for m,image,p,r,b,n,g in re.findall(r'mode=(\w+) image=(0x[\da-f]+) pages=(\d+) rebases=(\d+) binds=(\d+) native_ns=(\d+) guest_ns=(\d+)', text)]
        row['fixups'] = metrics
        rows.append(row)
        (root/'measurements.json').write_text(json.dumps(rows, indent=2)+'\n')
        if r.returncode: raise AssertionError(f'{name}: rc={r.returncode}; see stderr')
        if variant in ('off', 'precedence'): assert 'nativefixups:' not in text, name
        if variant in ('on', 'verify') and 'trivial' not in name:
            assert metrics and sum(m['rebases']+m['binds'] for m in metrics)>0, f'{name}: path not exercised'
            assert all(m['mode']==('verified' if variant=='verify' else 'native') for m in metrics), name
            assert 'fallback:' not in text, name
        return r.stdout
    # Warm only the existing private DSC disk cache, no translated AOT.
    run('warmup', 'off', [trivial])
    for workload, (cmd, expected) in workloads.items():
        outputs = []
        for mode in ['off', 'verify', 'on', 'precedence']:
            outputs.append(run(f'check-{workload}-{mode}', mode, cmd))
        assert len(set(outputs)) == 1, f'{workload}: stdout differs'
        assert expected.encode() in outputs[0]
        print(f'PASS {workload}: OFF=shadow dyld=actual native=OFF precedence', flush=True)
    # Interpreter control on the synthetic dylib: no translator test matrix needed.
    run('check-big-interp', 'on', workloads['big'][0], opts=('--interp',))
    # Verify static prewarm sees the same patched entry, then use the AOT twice.
    warmenv = {**env, 'ATTESOR_JIT_ON':'nativefixups', 'ATTESOR_AOT_SYNC':'1'}
    warmenv.pop('ATTESOR_NO_AOT_BUILD', None)
    r = subprocess.run([str(binary), '--prewarm', str(foundation)], env=warmenv, capture_output=True, timeout=180)
    (root/'prewarm.stderr').write_bytes(r.stderr)
    assert r.returncode==0 and b'nativefixups: installed' in r.stderr, 'prewarm failed'
    for i in range(2): run(f'check-foundation-aot-{i}', 'on', [foundation], opts=())
    for pair in range(args.pairs):
        for workload, (cmd, _) in workloads.items():
            for mode in ['off', 'on']:
                run(f'ab-{pair}-{workload}-{mode}', mode, cmd)
    # Profiles separate from wall measurements; retain full records and uptime.
    for mode in ['off', 'on']:
        run(f'profile-node-{mode}', mode, workloads['node'][0], extra={'ATTESOR_PROFILE':str(root/f'profile-{mode}')})
    run('check-process-fork', 'on', [ELECTRON, REPO/'tests/attesor/process-fork.js'])
    assert b'PASS fork: 6 children, env argv IPC exit' in (root/'check-process-fork.stdout').read_bytes()
    # Change only a PRIVATE copy's arm64 dyld UUID: it must run unhooked.
    dyld = Path('/usr/lib/dyld')
    data = bytearray(dyld.read_bytes())
    offset = 0
    if data[:4] == bytes.fromhex('cafebabe'):
        slices = []
        for i in range(struct.unpack_from('>I', data, 4)[0]):
            cpu, sub, off, size, align = struct.unpack_from('>IIIII', data, 8+20*i)
            if cpu == 0x100000c: slices.append((sub & 255 == 2, off))
        offset = sorted(slices, reverse=True)[0][1]
    cmd = offset+32
    changed = False
    for i in range(struct.unpack_from('<I', data, offset+16)[0]):
        kind, size = struct.unpack_from('<II', data, cmd)
        if kind == 0x1b:
            data[cmd+8:cmd+24] = bytes(16)
            changed = True
        cmd += size
    assert changed
    copy = root/'unknown-dyld'
    copy.write_bytes(data)
    output = run('check-unknown-dyld', 'unsupported', workloads['node'][0], opts=('--no-aot', '--dyld', str(copy)))
    assert output == (root/'check-node-off.stdout').read_bytes()
    assert 'disabled: unaudited dyld UUID' in (root/'check-unknown-dyld.stderr').read_text()
    print('PASS fork children and unknown-dyld fallback')
    print(f'PASS: directed checks and {args.pairs} alternating A/B pairs; raw {root}/measurements.json')


if __name__ == '__main__':
    main()
