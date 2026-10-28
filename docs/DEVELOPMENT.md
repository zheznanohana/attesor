# Development notes

Open problems, the bug list (so you recognise symptoms), and a short history.

## Remaining hot spots / next steps (rough priority)

1. **JIT code quality** (still the biggest lever): the first round (fusion, dead flag stores, a write-through
   register cache inside a block) is done, see "JIT code quality work". Remaining: registers still reload at every
   block boundary (a loop iteration is one block), the TBI mask (`shl/shr 8`) on every access, block-exit/IBTC cost,
   flags kept as four bytes.
2. Still-interpreted instructions. Done in Attesor: SHA-256, `dc` cache ops (DC ZVA native; the IC IVAU
   terminator mask used to catch every DC op), TBL/TBX, FRINT*, FCVTZU, UCVTF/SCVTF vector,
   [US]ADDL/ADDW/SUBL/SUBW/MLAL/MLSL, SUBHN, EXT, SQ(R)DMULH (16-bit), [US]AB[DA], [US]ADDLP/ADALP,
   FCVTL/FCVTN (F16C for half), FMLA/FMLS/FMUL by element, vector FMLA/FMLS and scalar FMADD family
   (FMA3; both detected at run time, else interpreted). Left, by DeepSeek Harness profile: USRA/SSRA,
   64-bit UZP/ZIP, FMAXNM/FMINNM, FCVTMS and other rounding FCVT*, SQXTUN, UQSHRN, UADDLV/ADDV/
   UMAXP/SMINV reductions, PMULL, PACGA, MUL/MLA by element, LDXR/STXR exclusives, FP16 arithmetic.
   Use `ATTESOR_FALLBACK_HIST` on the GPU (`--type=gpu-process`), renderer and helper processes to see
   what matters, and `bench/app-startup.sh` to measure (cold mode isolates the JIT).
3. Startup: work is ~10 processes on 4 cores; per-process translation. Ideas: pre-warm AOT,
   fewer VS Code processes/features, check Chromium/Electron flags (`--in-process-gpu`,
   `--disable-features=…`), persistent V8 code cache (don't wipe the profile dir).
4. Chromium-specific: run with a real profile; check `about:blank` page target had `url ""`
   for a long time in one run (renderer for the initial page slow to start) — investigate.
5. GPU: done for Chromium via the Metal bridge (see "GPU passthrough"); apps that present through
   `CAMetalLayer`/`MTKView` directly still need that bridged.
6. Robustness: stale-IBTC hazard if a thread only ever hits IBTC entries after invalidation
   (accepted risk); AOT profile saving only on graceful exit; concurrent builders write the same
   files (last writer wins, atomic rename); `ATTESOR_SPIN_DEBUG` for infinite-dispatch bugs.
7. Overcast / iOS-on-Mac apps: would need the iOS-on-Mac runtime and unencrypted binaries;
   FairPlay-encrypted App Store binaries are not runnable off Apple Silicon and circumventing DRM
   is out of scope. Suggested alternative: build your own UIKit "Designed for iPad" or Mac
   Catalyst app.

## Key bugs found & fixed (so you recognise symptoms)

- `--sysroot` was parsed but never used; dyld walks paths via dirfd (fixed with vpath tracking).
- Host rejects arm64 code signatures (errno 85) → emulate `fcntl` code-sign ops.
- libobjc aborts on `task_restartable_ranges_register` → patch mach_msg2 reply.
- Non-canonical address GP faults from TBI-tagged pointers → mask in interp and JIT.
- IOKit: MobileGestalt waits forever for `AppleDiagnosticDataAccessReadOnly` → fake service+iterator.
- libdispatch timers never fire → event-manager QoS lost on fired events.
- Two event-manager threads → libdispatch "Locking the manager should not fail".
- `bsdthread_ctl` EINVAL → libdispatch "_pthread_set_properties_self failed".
- 4K vs 16K pages → V8 allocator crash (`munmap` EINVAL) → 16K mmap.
- `posix_spawn` of arm64 helpers failed on the Intel kernel → re-launch through attesor.
- JIT cache exhausted / thrash from flush-all invalidation → per-unit invalidation, 4 GiB cache.
- Forked child spun forever (stale task port) → libc fork.
- Vacuous self-tests (fallback == interpreter) → report `native=` count.
- Wrong bit masks pinned the U bit for unsigned SIMD variants (umull/raddhn/ushr/ushll/uminv).
- AdvSIMD load/store were never routed to the SIMD translator (dispatched via `ldst`).
- LD4/ST4 element size is bits 11:10 (not 23:22).
- Two labels at the same address need a `nop` between (iced).
- Exec-mmap of AOT files triggers Gatekeeper dialogs → copy into the code cache.
- Chromium hang: profiler `thread_suspend` (msg id 3605) → no-op shim.


## Session log summary (chronological)

1. Obtained arm64e dyld cache (macOS 26.6.2) on the test Mac via `ipsw` partial IPSW extraction
   (26.7/25G229 not indexed). 2. Made fastfetch, then neatvi, run on the test Mac. 3. IOKit shim,
   FP16 NEON, dispatch timers, single event manager → AppKit calculator. 4. 16K pages, arm64
   child spawn, libc fork, precise invalidation → VS Code Electron boots. 5. Long performance
   campaign (native NEON/atomics, AOT for libs, invalidation) using VS Code as the benchmark.
6. `attesor ./binary` auto-sysroot. 7. Chromium snapshot downloaded to Downloads (both Macs),
   thread-suspend shim, Gatekeeper fix → Chromium renders.
