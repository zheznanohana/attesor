# Rosetta mechanism research

Owner-authorized, bounded compiler research. Only independently authored probes,
scripts, metadata and short analysis excerpts are published. Never export Apple
binaries, full decompilations, protected translation caches, credentials or local
application data. Do not disable SIP or use nested VMs/remote-access tunnels.

The research branch's initial push explicitly starts one bounded CI job; it is
not a recurring monitor. `workflow_dispatch` is available after the workflow is
introduced on the repository's default branch. Standard ARM64 runner only.

Sources to distinguish from current observations:

* Apple Platform Security: Rosetta 2 (public JIT/AOT and cache identity).
* Dougall Johnson, *Why is Rosetta 2 fast?* (2022-11-09; historical translated-code observations).
* FFRI Project Champollion parts 1/2 (2021; historical runtime/AOT analysis).

Static symbols, strings and decompiler p-code are evidence of code structure,
not proof of a particular optimizer. LLDB's guest x86 view is not ARM output.
Every claimed mechanism needs a version/hash/address and a reproducible source.
Borrow mechanisms only: ARM guests have more GPRs than an x86 host, so Rosetta's
reverse-direction fixed-register mapping cannot simply be reversed.

Local success remains all six original sub-60% workloads reaching >=60% of
same-host native x86 throughput: fp-nbody, matmul, fib(32), qsort, sieve, branchy.
No cloud score, mean score, algorithm substitution or checksum-only test replaces
this acceptance. Retain both qsort sizes as a regression check.
