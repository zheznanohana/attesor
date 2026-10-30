#!/usr/bin/env python3
"""Three serial native/off/on rounds, private cache, uptime, raw paired ratios."""
import argparse
import os
from pathlib import Path
import statistics
import subprocess
p=argparse.ArgumentParser()
p.add_argument("--out",type=Path,default=Path("target/native-round2-measure"))
p.add_argument("--attesor",type=Path,default=Path("target/release/attesor"))
a=p.parse_args();root=a.out.resolve();root.mkdir(parents=True,exist_ok=True)
env={k:v for k,v in os.environ.items() if not k.startswith("ATTESOR_") and k!="DYLD_INSERT_LIBRARIES"}
env.update(TMPDIR="/private/tmp",ATTESOR_CACHE=str(root/"cache"),ATTESOR_NATIVE_DIR=str(root/"native"))
subprocess.run(["native/build.sh"],env={**env,"OUT":str(root/"native")},check=True)
for arch in ("arm64","x86_64"):
    subprocess.run(["clang","-O2","-Wall","-Wextra","-Werror","-arch",arch,"tests/attesor/native-round2-bench.c","native/imageio-buffer.c","-framework","Accelerate","-framework","CoreFoundation","-framework","CoreGraphics","-framework","ImageIO","-lcompression","-o",str(root/arch)],env=env,check=True)
times={m:{} for m in ("native","off","on")}
for round in range(1,4):
    for mode in times:
        cmd=[str(root/"x86_64")] if mode=="native" else [str(a.attesor.resolve()),"--no-aot",str(root/"arm64")]
        with (root/f"{mode}-{round}.txt").open("w") as out,(root/f"{mode}-{round}.err").open("w") as err:
            subprocess.run(["uptime"],env=env,stdout=out,check=True);out.flush()
            subprocess.run(cmd,env={**env,"ATTESOR_NATIVE_LIBS":"vdsp,vimage,compression,crypto,imageio" if mode=="on" else "0"},stdout=out,stderr=err,check=True,timeout=300)
        counters=dict(s.split("=") for s in (root/f"{mode}-{round}.err").read_text().splitlines() if "=" in s)
        assert set(counters)=={"vdsp","vimage","compression","crypto","imageio"},counters
        for count in counters.values():assert (int(count)>0)==(mode=="on")
        for line in (root/f"{mode}-{round}.txt").read_text().splitlines():
            fields=line.split()
            if len(fields)==3 and fields[2]=="ms":times[mode].setdefault(fields[0],[]).append(float(fields[1]))
        print(f"completed {mode}-{round}",flush=True)
summary="API | native ms (3) | off ms (3) | on ms (3) | median paired off/on\n"
for api,native in times["native"].items():
    off,on=times["off"][api],times["on"][api]
    summary+=f"{api} | {native} | {off} | {on} | {statistics.median([x/y for x,y in zip(off,on)]):.3f}x\n"
(root/"summary.txt").write_text(summary);print(summary)
