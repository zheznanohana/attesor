#!/usr/bin/env python3
"""Explicit RGBX data adapter: PNG exact pixels, JPEG error/PSNR, 3x3 cross-codec."""
import argparse
import math
import os
from pathlib import Path
import subprocess


def psnr(a,b):
    assert len(a)==len(b) and len(a)%4==0
    diff=[abs(x-y) for i,(x,y) in enumerate(zip(a,b)) if i%4!=3]
    mse=sum(v*v for v in diff)/len(diff)
    return max(diff), math.inf if mse==0 else 10*math.log10(255**2/mse)


def main():
    p=argparse.ArgumentParser()
    p.add_argument("--out",type=Path,default=Path("target/native-imageio-check"))
    p.add_argument("--attesor",type=Path,default=Path("target/release/attesor"))
    args=p.parse_args();root=args.out.resolve();root.mkdir(parents=True,exist_ok=True)
    env={k:v for k,v in os.environ.items() if not k.startswith("ATTESOR_") and k!="DYLD_INSERT_LIBRARIES"}
    env.update(TMPDIR="/private/tmp",ATTESOR_CACHE=str(root/"cache"),ATTESOR_NATIVE_DIR=str(root/"native"))
    subprocess.run(["native/build.sh"],env={**env,"OUT":str(root/"native")},check=True)
    for arch in ("arm64","x86_64"):
        subprocess.run(["clang","-arch",arch,"-O2","-Wall","-Wextra","-Werror","tests/attesor/native-imageio-buffer.c","native/imageio-buffer.c","-framework","CoreFoundation","-framework","CoreGraphics","-framework","ImageIO","-o",str(root/arch)],env=env,check=True)
    def run(mode,task,label,*extra):
        cmd=[str(root/"x86_64")] if mode=="native" else [str(args.attesor.resolve()),"--no-aot",str(root/"arm64")]
        with (root/f"{label}.out").open("wb") as out,(root/f"{label}.err").open("wb") as err:
            subprocess.run(cmd+[task,*map(str,extra)],env={**env,"ATTESOR_NATIVE_LIBS":"imageio" if mode=="on" else "0"},stdout=out,stderr=err,check=True,timeout=300)
        count=next(int(v.split("=")[1]) for v in (root/f"{label}.err").read_text().splitlines() if v.startswith("imageio-calls="))
        assert (count>0)==(mode=="on"),(label,count)
        print(f"PASS {label} imageio-calls={count}",flush=True)
    for encoder in ("native","off","on"):
        directory=root/f"encoded-{encoder}";directory.mkdir(exist_ok=True)
        run(encoder,"encode",f"encode-{encoder}",directory)
        for decoder in ("native","off","on"):
            output=root/f"decoded-{encoder}-{decoder}";output.mkdir(exist_ok=True)
            run(decoder,"decode",f"decode-{encoder}-{decoder}",directory,output)
            for raw in directory.glob("*.raw"):
                original=raw.read_bytes();base=raw.stem
                assert (output/f"{base}.png.raw").read_bytes()==original,(encoder,decoder,base,"PNG pixels")
                d=(output/f"{base}.jpg.raw").read_bytes()
                assert len(d)==len(original) and all(d[i]==255 for i in range(3,len(d),4))
                error,quality=psnr(original,d)
                print(f"JPEG reconstruction {encoder}/{decoder}/{base}: max_byte={error} PSNR={quality:.6f} dB")
        # Same stream decoded on host vs guest: fixed 4-level per-channel budget.
        for path in (root/f"decoded-{encoder}-native").glob("*.jpg.raw"):
            for decoder in ("off","on"):
                error,quality=psnr(path.read_bytes(),(root/f"decoded-{encoder}-{decoder}"/path.name).read_bytes())
                assert error<=4,(encoder,decoder,path.name,error,quality)
                print(f"JPEG decoder {encoder}/{decoder}/{path.name}: max_byte={error} PSNR={quality:.6f} dB")
    # Encoder acceptance: compare each stream through the SAME host decoder.
    for path in (root/"decoded-off-native").glob("*.jpg.raw"):
        error,quality=psnr(path.read_bytes(),(root/"decoded-on-native"/path.name).read_bytes())
        assert quality>=35,(path.name,error,quality)
        print(f"JPEG encoder off/on {path.name}: max_byte={error} PSNR={quality:.6f} dB")
    for mode in ("native","off","on"):run(mode,"threads",f"threads-{mode}")
    print("native-imageio buffer suite: PASS")

if __name__=="__main__":main()
