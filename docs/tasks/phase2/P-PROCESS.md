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
