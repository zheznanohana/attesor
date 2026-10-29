#!/usr/bin/env python3
"""Headless Round 2 gates: complete bytes, FP errors, cross-decoder artifacts.
No third-party Python modules. Logs and private caches stay under --out.
"""
import argparse
import math
import os
from pathlib import Path
import struct
import subprocess

ALL = "vdsp,vimage,compression,crypto"
SYMBOLS = [f"attesor_native_{s}_calls" for s in ("vdsp", "vimage", "compression", "crypto")]

def records(path):
    data = path.read_bytes()
    offset = 0
    while offset < len(data):
        assert offset + 40 <= len(data), (path, offset, "truncated header")
        name = data[offset:offset+32].split(b"\0")[0].decode()
        kind, size = struct.unpack_from("<II", data, offset+32)
        offset += 40
        assert offset+size <= len(data), (path, name, "truncated payload")
        yield name, kind, data[offset:offset+size]
        offset += size

def oracle(path):
    rows=list(records(path)); checked=0
    sentinel=struct.unpack("<f",bytes.fromhex("a5a5a5a5"))[0]
    for i,(name,kind,payload) in enumerate(rows):
        if name!="fft-input-real":continue
        re=struct.unpack(f"<{len(payload)//4}f",payload)
        im=struct.unpack(f"<{len(payload)//4}f",rows[i+1][2])
        stride=2 if re[1]==sentinel else 1
        n=len(re)//stride
        out_re=struct.unpack(f"<{len(payload)//4}f",rows[i+2][2])
        out_im=struct.unpack(f"<{len(payload)//4}f",rows[i+3][2])
        norm=sum(abs(re[j*stride])+abs(im[j*stride]) for j in range(n))
        budget=max(32*2**-23*max(1,math.log2(n))*norm, 8*n*max(1,math.log2(n))*2**-149)
        if n<=32:
            for k in range(n):
                reference=sum(complex(re[j*stride],im[j*stride])*complex(math.cos(-2*math.pi*j*k/n), math.sin(-2*math.pi*j*k/n)) for j in range(n))
                assert abs(out_re[k*stride]-reference.real)<=budget,(path,k,"DFT real")
                assert abs(out_im[k*stride]-reference.imag)<=budget,(path,k,"DFT imag")
            checked+=1
        inv_re=struct.unpack(f"<{len(payload)//4}f",rows[i+4][2])
        inv_im=struct.unpack(f"<{len(payload)//4}f",rows[i+5][2])
        for j in range(n):
            assert abs(inv_re[j*stride]-n*re[j*stride])<=n*budget,(path,j,"inverse real")
            assert abs(inv_im[j*stride]-n*im[j*stride])<=n*budget,(path,j,"inverse imag")
    assert checked==20,(path,checked)
    print(f"PASS independent double DFT {path.name}: {checked} transforms; all FFT inverse identities")


def compare(left, right, *, native=False):
    a, b = list(records(left)), list(records(right))
    assert len(a) == len(b), (left, right, len(a), len(b))
    stats = {}
    for index, ((name, kind, x), (name2, kind2, y)) in enumerate(zip(a, b)):
        assert (name, kind, len(x)) == (name2, kind2, len(y))
        if native and name == "guest-scale8":
            continue  # known integer cross-architecture difference, not bridged
        if kind == 2:
            assert x == y, (name, "bytes differ", left, right)
            continue
        assert kind == 1 and len(x) % 4 == 0
        xx = struct.unpack(f"<{len(x)//4}f", x)
        yy = struct.unpack(f"<{len(y)//4}f", y)
        # Scale-aware numerical budget. Relative max alone is not a
        # suitable gate near FFT zeros/cancellation. These are empirical QA
        # budgets, NOT a claim Apple promises a uniform cross-platform bound.
        sentinel = struct.unpack("<f", bytes.fromhex("a5a5a5a5"))[0]
        scale = max((abs(v) for v in xx if math.isfinite(v) and v != sentinel), default=0)
        if "fft" in name and (name.endswith("-real") or name.endswith("-imag")):
            # Complex error is relative to the COMPLEX transform, not the
            # roundoff-only imaginary component of an otherwise real result.
            neighbor = a[index + (1 if name.endswith("-real") else -1)][2]
            vals = struct.unpack(f"<{len(neighbor)//4}f", neighbor)
            scale = max(scale, max((abs(v) for v in vals if math.isfinite(v) and v != sentinel), default=0))
        budget = max(2e-6 * scale, 32 * 2**-149)
        if "fft" in name or name in ("dot", "sum"):
            budget = max(2e-5 * scale, 32 * 2**-149)
            if "fft" in name:
                # Four successive unnormalized transforms amplify underflow
                # roundoff. Bound that separately from normal-range relative
                # error, rather than hiding subnormals with a unit absolute floor.
                budget = max(budget, 8 * len(xx)**2 * max(1, math.log2(len(xx))) * 2**-149)
        for u, v in zip(xx, yy):
            if u == sentinel:
                assert u == v, (name, "padding modified")
                continue
            if not math.isfinite(u) or not math.isfinite(v):
                assert (math.isnan(u) and math.isnan(v)) or u == v, (name, u, v)
                continue
            if name in ("special-add","special-mul") and u==0 and v==0:
                assert math.copysign(1,u)==math.copysign(1,v),(name,"signed zero")
            diff = abs(u-v)
            rel = diff/max(abs(u), abs(v), 2**-149)
            st = stats.setdefault(name, [0., 0., 0])
            st[0], st[1], st[2] = max(st[0], diff), max(st[1], rel), st[2]+1
            assert diff <= budget, (name, u, v, diff, budget, left, right)
    for name, (absolute, relative, count) in stats.items():
        print(f"ERROR {name}: max_abs={absolute:.9g} max_rel={relative:.9g} finite_values={count}")
    print(f"PASS compare {left.name} / {right.name}: {len(a)} complete records")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--out", type=Path, default=Path("target/native-round2-check"))
    p.add_argument("--attesor", type=Path, default=Path("target/release/attesor"))
    args = p.parse_args()
    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=True)
    env = {k:v for k,v in os.environ.items() if not k.startswith("ATTESOR_") and k!="DYLD_INSERT_LIBRARIES"}
    env.update(TMPDIR="/private/tmp", ATTESOR_CACHE=str(root/"cache"), ATTESOR_NATIVE_DIR=str(root/"native"), ATTESOR_GPU_DIR=str(root/"host"))
    subprocess.run(["native/build.sh"], env={**env, "OUT":str(root/"native")}, check=True)
    for arch in ("arm64", "x86_64"):
        subprocess.run(["clang", "-O2", "-Wall", "-Wextra", "-Werror", "-arch", arch,
                        "tests/attesor/native-round2.c", "-framework", "Accelerate", "-lcompression", "-o",str(root/arch)], env=env, check=True)

    (root/"host").mkdir(exist_ok=True)
    subprocess.run(["clang","-O2","-arch","x86_64","tests/attesor/native-round2-helper.c","-o",str(root/"helper")],env=env,check=True)
    subprocess.run(["clang","-O2","-arch","x86_64","-dynamiclib","gpu/attesorexec.c","-o",str(root/"host/libattesorexec.dylib")],env=env,check=True)

    def run(mode, task, label, extra=(), seed=1):
        libs = {"native":"0", "off":"0", "on":ALL}.get(mode, mode)
        cmd = [str(root/"x86_64")] if mode == "native" else [str(args.attesor.resolve()), "--no-aot", str(root/"arm64")]
        output, error = root/f"{label}.bin", root/f"{label}.err"
        with output.open("wb") as out, error.open("wb") as err:
            subprocess.run(cmd+[task, *(str(v) for v in extra or ("unused",)), str(seed)], env={**env,"ATTESOR_NATIVE_LIBS":libs}, stdout=out, stderr=err, check=True, timeout=180)
        counters = dict(line.split("=") for line in error.read_text().splitlines() if line.startswith("attesor_native_"))
        assert set(counters) == set(SYMBOLS), (error, counters)
        for symbol, count in counters.items():
            selected = symbol.removeprefix("attesor_native_").removesuffix("_calls") in libs.split(",")
            touched = task in ("threads","chain") or (task=="numeric" and symbol in SYMBOLS[:2]) or (task=="fork" and symbol==SYMBOLS[0]) or (task=="crypto" and symbol==SYMBOLS[3]) or (task in ("encode","decode") and symbol==SYMBOLS[2])
            assert (int(count)>0) == (selected and touched), (label, symbol, count, libs)
        print(f"PASS {label}: {counters}", flush=True)
        return output

    for seed in (1,42,0xdeadbeef):
        n = run("native", "numeric", f"numeric-native-{seed}", seed=seed)
        off = run("off", "numeric", f"numeric-off-{seed}", seed=seed)
        on = run("on", "numeric", f"numeric-on-{seed}", seed=seed)
        for output in (n,off,on):oracle(output)
        compare(off,on)
        compare(n,on,native=True)
        for lib in ("vdsp","vimage"):
            only=run(lib,"numeric",f"numeric-{lib}-{seed}",seed=seed)
            compare(off,only)
        for mode in ("native","off","on","crypto"):
            output=run(mode,"crypto",f"crypto-{mode}-{seed}",seed=seed)
            if mode=="native": reference=output
            else: compare(reference,output)
    for mode in ("native","off","on","compression"):
        artifact=root/f"encoded-{mode}.dat"
        run(mode,"encode",f"encode-{mode}",(artifact,))
        for decoder in ("native","off","on"):
            run(decoder,"decode",f"decode-{mode}-{decoder}",(artifact,))
    for mode in ("native","off","on", "vdsp", "vimage", "compression", "crypto"):
        run(mode,"threads",f"threads-{mode}")
    for mode in ("native","off","on"):run(mode,"fork",f"fork-{mode}")
    for task in ("chain","chain-off"):
        run("on",task,task,(root/"helper",))
    for lib in ALL.split(","):
        result=subprocess.run([str(args.attesor.resolve()),"--no-aot",str(root/"arm64"),"threads"],env={**env,"ATTESOR_NATIVE_LIBS":lib,"ATTESOR_NATIVE_DIR":str(root/"missing")},capture_output=True)
        assert result.returncode!=0 and b"missing; run native/build.sh" in result.stderr
    print("native-round2 suite: PASS")

if __name__=="__main__":
    main()
