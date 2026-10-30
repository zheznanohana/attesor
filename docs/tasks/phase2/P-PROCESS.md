# Track P — process level: shared translations (#28), zygote (#29), dyld closures (#30), OS emulation (#23, #24), QoS (#35)
Branch `process`, worktree `~/attesor-p2p`. Read COMMON.md. Files: spawn.rs, threads.rs, syscalls.rs, loader.rs,
aot.rs/cache (sharing), new modules. Avoid the translator core (emit.rs); only call its public API.
Evidence: Electron apps start 6+ guest processes; each translates the same Electron Framework (cold start:
1.37 M blocks, 58.7 CPU-s translating across processes). Per-process start costs ~450 ms (dyld, cache map).
1. #28 Shared live translation cache between processes of the same app (same images, same translator build):
   e.g. a shared-memory segment keyed like the AOT files where processes publish translated blocks (position-
   independent Plain units, like AOT) and others map them; safe concurrent publication; invalidation stays
   per process. Measure blocks translated per process and CPU-s, cold DSH-like multi-process workload
   (use headless Node children: `child_process.fork` many times) before/after.
2. #29 Zygote: a warm attesor template process (cache mapped, slide applied, AOT loaded, dyld ready) that forks
   per new guest process instead of exec'ing a fresh attesor; opt-in; correct handling of fds, env, argv,
   signals, ports. Measure spawn latency of a trivial arm64 program and of `node -e 0` (DSH binary as Node).
3. #30 Persist dyld's launch work if it is repeatable (inspect what guest dyld does per launch under the
   interpreter/JIT; e.g. cache PrebuiltLoader sets or the result of its expensive steps). Report if not feasible.
4. #23/#24: measure emulation overhead of Mach IPC, psynch/ulock, path mapping/container mapping; fix if >5%.
5. #35: translation threads at lower QoS, guest UI threads unchanged; measure that nothing gets slower.

## Round 2 (Claude)
Round 1 committed (measurements, opt-in process-qos: slower, stays off). Decisions:
- #28 moves to wave 2 (needs translator-core registration/invalidation APIs; it will be done together with the
  core tracks). Do not work on it now.
- #30, new approach: your Node profile shows dyld's `JustInTimeLoader::applyFixups` at ~24% of start-up
  execution samples. Apply chained fixups natively instead of caching closures: recognise the guest dyld's
  fixup-application entry point(s) for an image (dyld has symbols in the sysroot), and perform the same
  rebases/binds in Rust on the host for that image (the way `cache::map_and_slide_2` applies shared-cache
  slides), returning to dyld as if its loop had run. Binds need symbol resolution: either keep dyld doing binds
  and do only rebases natively, or resolve via dyld's own results; measure which part dominates first.
  Opt-in `ATTESOR_JIT_ON=nativefixups` (or a process option), exact equivalence check: memory of every fixed-up
  page identical with and without, for Node, a Foundation test program and a big dylib; start time A/B.
- #29, safer design to evaluate (feasibility report first, implement only if the hazards are solvable): spawn
  without exec — the parent attesor forks itself and the child drops the parent's guest state and loads the new
  program in-process, reusing the mapped shared cache, slide cache and loaded AOT. Parent/child, waitpid,
  signals and fds then stay correct by construction; the open problems are multithreaded fork safety (locks of
  other threads, allocator), posix_spawn file actions/attributes, and fully unmapping the parent's guest memory.
