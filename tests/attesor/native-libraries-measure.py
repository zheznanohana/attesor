#!/usr/bin/env python3
"""Reproducible serial alternating survey or convolution A/B. No shared caches."""
import argparse
import os
from pathlib import Path
import subprocess
import statistics

p = argparse.ArgumentParser()
p.add_argument("--attesor", type=Path, default=Path("target/release/attesor"))
p.add_argument("--out", type=Path, default=Path("target/n-native-measure"))
p.add_argument("--bridge", action="store_true", help="warm convolution off/on, rather than initial survey")
args = p.parse_args()
root = args.out.resolve()
root.mkdir(parents=True, exist_ok=True)
env = {k: v for k, v in os.environ.items() if not k.startswith("ATTESOR_") and k != "DYLD_INSERT_LIBRARIES"}
env.update(TMPDIR="/private/tmp", ATTESOR_CACHE=str(root / "cache"))
source = "native-vimage-bench.c" if args.bridge else "native-libraries-bench.c"
frameworks = ["Accelerate"] if args.bridge else ["Accelerate", "CoreFoundation", "CoreGraphics", "ImageIO"]
for arch in ("arm64", "x86_64"):
    cmd = ["clang", "-w", "-O2", "-arch", arch, "tests/attesor/" + source]
    for f in frameworks:
        cmd += ["-framework", f]
    if not args.bridge:
        cmd += ["-lcompression"]
    subprocess.run(cmd + ["-o", str(root / arch)], env=env, check=True)
if args.bridge:
    subprocess.run(["native/build.sh"], env={**env, "OUT": str(root / "native")}, check=True)
for i in range(1, 4):
    modes = ("native", "off", "on") if args.bridge else ("native", "off")
    for mode in modes:
        extra = {"ATTESOR_NATIVE_LIBS": "vimage" if mode == "on" else "0", "ATTESOR_NATIVE_DIR": str(root / "native")}
        cmd = [str(root / "x86_64")] if mode == "native" else [str(args.attesor.resolve()), "--no-aot", str(root / "arm64")]
        with (root / f"{mode}-{i}.txt").open("w") as out, (root / f"{mode}-{i}.err").open("w") as err:
            subprocess.run(["uptime"], stdout=out, env=env, check=True)
            out.flush()
            subprocess.run(cmd, stdout=out, stderr=err, env={**env, **extra}, check=True, timeout=300)
        print(f"completed {mode}-{i}", flush=True)
if args.bridge:
    hashes = set()
    times = {m: [] for m in ("native", "off", "on")}
    for mode in times:
        for i in range(1, 4):
            lines = (root / f"{mode}-{i}.txt").read_text().splitlines()
            times[mode].append(float(next(s for s in lines if s.startswith("vImage-convolve-")).split()[1]))
            hashes.add(next(s for s in lines if s.startswith("bytes=")))
            assert (root / f"{mode}-{i}.err").read_text().strip() == f"bridge-calls={129 if mode == 'on' else 0}"
    assert len(hashes) == 1, hashes
    ratios = [a / b for a, b in zip(times["off"], times["on"])]
    summary = f"ms={times}\npaired off/on={ratios}\nmedian paired speedup={statistics.median(ratios):.3f}x\nidentical {hashes.pop()}\n"
else:
    times = {m: {} for m in ("native", "off")}
    for mode in times:
        for i in range(1, 4):
            for line in (root / f"{mode}-{i}.txt").read_text().splitlines():
                fields = line.split()
                if len(fields) == 3 and fields[1] != "SKIP":
                    times[mode].setdefault(fields[0], []).append(float(fields[1]))
    summary = "API | native ms (3) | translated ms (3) | ratio of medians\n"
    for name, native in times["native"].items():
        guest = times["off"][name]
        summary += f"{name} | {native} | {guest} | {statistics.median(guest)/statistics.median(native):.3f}x\n"
(root / "summary.txt").write_text(summary)
print(summary)
