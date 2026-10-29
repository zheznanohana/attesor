# DSH profile, round 1 (Claude, 2026-10-29 18:45-18:55, ~/bin/attesor at 35ce7cd, machine load 5-8)
Commands: ~/attesor-dshprof/run.sh <name> <cache> <secs-after-ready>; report: tools/profile-report.py <name>/profile

| run | ready | CPU at stop | translated blocks / ms |
|---|---|---|---|
| cold (empty cache), stop 40 s after ready | 32.9 s | 274 s | 1,368,469 / 58,750 |
| "warm" after a 73 s session (Electron AOT not yet built) | 31.3 s | 137 s | 1,061,678 / 48,119 |
| true warm (after a session long enough for the Electron AOT: built ~160 s in) | 30.0 s | 170 s | 215,808 / 7,675 |

Busy-time split (idle excluded), true warm: syscall-bsd 30%, jit 30%, gpu 20%, dispatch 14%, translate 4%, interp 1%.
Per process (true warm, thread-seconds): main process syscall-bsd 48.4 s vs jit 26.6 s; GPU process gpu 37.0 s,
jit 13.1 s; renderer jit 14.5 s, dispatch 7.2 s.
Exec share (jit+interp) by image, true warm: Electron Framework 65%, dyld 9%, unknown 7%, CoreFoundation 6%,
QuartzCore 2%, libobjc 1.6%, libswiftCore 1.1%, malloc 0.8% (system libraries together ~10% of exec time).
Conclusions: (1) library thunks can win at most ~2-3% of DSH busy time; Electron (65% of exec) is statically
linked. (2) The largest emulation overheads are BSD syscalls (main process) and the GPU bridge.
(3) The AOT build of Electron finishes only ~160 s into a session; shorter sessions leave the next launch cold.
