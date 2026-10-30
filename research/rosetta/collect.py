#!/usr/bin/env python3
"""Bounded local/cloud evidence collection. No uploading Apple binaries/caches."""
import hashlib,json,os,pathlib,subprocess,sys,time

ROOT=pathlib.Path(__file__).resolve().parent
OUT=pathlib.Path(sys.argv[1]).resolve();OUT.mkdir(parents=True,exist_ok=True)
BUILD=pathlib.Path(os.environ.get('RUNNER_TEMP','/private/tmp'))/'attesor-rosetta-probes'
BUILD.mkdir(exist_ok=True)
manifest={'status':'running','commands':[],'components':[],'limitations':[
    'Not an Attesor benchmark; cloud hardware differs from the Intel host.',
    'LLDB may present reconstructed x86 instructions, not actual translated ARM code.',
    'ROSETTA_PRINT_IR is an undocumented historical feature, availability is tested, not assumed.',
    'No protected AOT cache access, security setting changes or Apple binary redistribution.']}
def save(): (OUT/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
def run(name,command,env=None,timeout=60,required=False):
    start=time.monotonic()
    try:
        r=subprocess.run(command,capture_output=True,text=True,env=env,timeout=timeout)
        row={'name':name,'command':command,'returncode':r.returncode,'seconds':time.monotonic()-start}
        stdout,stderr=r.stdout,r.stderr
    except subprocess.TimeoutExpired as e:
        row={'name':name,'command':command,'timeout':timeout,'returncode':124};stdout=str(e.stdout or '');stderr=str(e.stderr or '')
    for suffix,data in [('stdout',stdout),('stderr',stderr)]:
        data=data.replace(str(pathlib.Path.home()),'<HOME>')
        (OUT/(name+'.'+suffix+'.txt')).write_text(data[:2*1024*1024])
    manifest['commands'].append(row);save()
    if required and row['returncode']:raise RuntimeError(name+' failed: '+stderr[-1000:])
    return row,stdout,stderr
try:
    run('host',['/usr/bin/uname','-a']);run('os',['/usr/bin/sw_vers'])
    run('hardware',['/usr/sbin/sysctl','-n','hw.machine','machdep.cpu.brand_string'])
    if subprocess.check_output(['uname','-m'],text=True).strip()!='arm64':raise RuntimeError('ARM64 runner required')
    for arch in ['arm64','x86_64']:
        run('compile-'+arch,['/usr/bin/clang','-arch',arch,'-O2','-ffp-contract=off',str(ROOT/'probe.c'),'-o',str(BUILD/('probe-'+arch))],required=True)
    _,native,_=run('native',[str(BUILD/'probe-arm64')],required=True)
    _,translated,_=run('rosetta',[str(BUILD/'probe-x86_64'),'--require-rosetta'],required=True)
    if native.splitlines()[1:]!=translated.splitlines()[1:]:raise RuntimeError('native/Rosetta output mismatch')
    manifest['probe_checksums_match']=True
    run('jit-ir',[str(BUILD/'probe-x86_64'),'--require-rosetta'],env={**os.environ,'ROSETTA_PRINT_IR':'1'})
    run('debugger',['/usr/bin/lldb','-b','-o','breakpoint set --name accumulate','-o','run','-o','register read','-o','disassemble --frame','-o','quit','--',str(BUILD/'probe-x86_64')],timeout=40)
    for parent in ['/usr/libexec/rosetta','/Library/Apple/usr/libexec/oah']:
        folder=pathlib.Path(parent)
        if not folder.exists():continue
        for f in sorted(folder.iterdir()):
            if not f.is_file() or f.is_symlink():continue
            row={'path':str(f),'bytes':f.stat().st_size,'sha256':hashlib.sha256(f.read_bytes()).hexdigest()}
            manifest['components'].append(row)
            tag=f.name.replace('.','_')
            run('format-'+tag,['/usr/bin/file',str(f)])
            run('signature-'+tag,['/usr/bin/codesign','-dv','--verbose=2',str(f)])
            # Symbol names only; do not export full Apple disassembly.
            run('symbols-'+tag,['/usr/bin/nm','-g',str(f)])
    manifest['status']='passed';save()
except BaseException as e:
    manifest.update(status='failed',error=str(e));save();raise
