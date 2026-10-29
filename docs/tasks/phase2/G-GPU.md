# Track G — GPU bridge (#25) and VideoToolbox hardware bridge (#27)
Branch `gpubridge`, worktree `~/attesor-p2g`. Read COMMON.md and docs/GPU.md. Files: `gpu/` (guest
libmclmetal.m, host mclbridge.m), `crates/attesor/src/gpu.rs`. Do not edit the translator core.
Evidence: in DeepSeek Harness's GPU process the bridge is 20% of busy time (37 thread-seconds in a 50 s run);
the profiler now reports bridge operations and selectors (`ATTESOR_PROFILE`, `tools/profile-report.py`).
VideoToolbox runs in software in the guest (HEVC encode of 60 720p frames 2.1 s vs 0.15 s native).
1. Measure: write a headless Metal workload (e.g. many small command buffers, buffer uploads, texture
   uploads, render passes to an offscreen texture) and profile the bridge: top selectors, calls, bytes copied,
   round trips per frame. Report before changing anything.
2. #25: reduce round trips and copies (batch more calls per message, shared-memory buffers for uploads,
   avoid synchronous replies where the result is not needed); keep the wire protocol versioned.
   Verify: tests/attesor rtprobe.m, metallayer/mtkview tests, the families probe (identical output), and the
   workload's throughput A/B.
3. #27: bridge VTCompressionSession / VTDecompressionSession to the host (IOSurface-backed CVPixelBuffers
   shared by IOSurface ID; encoded data as bytes + format description parameter sets). Opt-in
   `ATTESOR_GPU_VT=1`. Verify with tests/attesor/vtcodec.m: identical round-trip checks, hw=1, timings.
No GUI apps; offscreen rendering only.

## Round 2 (Claude ran the GPU measurements outside the sandbox: Metal/VT are unavailable inside it)
Baseline from `gpu/measure-headless.py --frames 120 --seed 1`, ~/bin/attesor, load 6-12:
native 339.9 fps; attesor 259.4 / 270.5 / 246.3 fps (~76% of native); checksum identical (fc302ac9fe269bef);
host calls per frame incl. setup 63.8 — by kind: 1:403, 2:403, 4:6836, 5:16 (120 frames).
Your job now: implement #25 (cut round trips: batch kind-4 calls, async where no reply is needed, shared memory
for uploads) and #27 (VT bridge, opt-in `ATTESOR_GPU_VT=1`) in code; keep `measure-headless.py --after` ready.
You cannot run Metal or VT in the sandbox: build, run the oracle/unit tests you can, then stop and say exactly
which commands Claude must run outside the sandbox (measure-headless.py --before/--after, tests/attesor rtprobe,
vtcodec). Commit messages to target/ as before.
