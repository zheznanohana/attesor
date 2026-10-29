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
