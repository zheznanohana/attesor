# Pure path-mapping cache (Track P / #24)

`ATTESOR_JIT_ON=pathcache` selects an experimental, thread-local cache for the
existing absolute dyld-path-to-sysroot string mapping. The exact token
`ATTESOR_JIT_OFF=pathcache` takes precedence. It is **off by default**.

The setting is read when `paths::set_cache_dir` configures the sysroot (normally
startup, via `--sysroot` or sysroot discovery). Merely changing environment
variables later is not a live configuration interface. Calling the setter again
publishes a new generation, even for an unchanged directory, invalidating old
answers in every thread. Generation exhaustion disables caching rather than
wrapping its identity.

Each thread retains at most 128 positive entries and 128 KiB of combined key and
value string payload. Hash-table/allocator overhead is additional, bounded by the
entry limit; there is no process-wide total memory budget. A full cache is cleared
before the next insertion. Keys and values exceeding 4096 bytes are not retained.
Returned values are still owned `String`s, as required by the existing API.

The cache stores **only lexical substitutions** for the three existing dyld
prefixes. It does not normalize `.`/`..`, inspect or resolve symlinks, cache
existence/permissions/stat results, discover containers, cache entitlements, or
read HOME. Unmatched and directly supplied relative paths are not retained.
Existing `openat` handling still constructs its absolute virtual path from the
current directory-fd mapping before calling `redirect`; no cwd or fd lookup is
cached here. Container selection and creation remain unchanged.

Cache hits avoid the existing global sysroot mutex and an extra sysroot-string
clone; misses use that same existing mutex and validate its paired generation.
No new global mutex or once-initialization lock is introduced. TLS teardown or an
already-borrowed TLS cache falls back to the existing uncached behavior.

A fork child does not reuse the parent's cache: until it explicitly configures
its own sysroot, the PID guard selects the old uncached path. A subsequent setter
and lookup also clear inherited TLS entries by PID/generation. This does **not**
repair preexisting fork safety of `CACHE_DIR`, fd-path maps, allocators or other
runtime locks; no general async-signal-safe post-fork contract is claimed.

The integ1 port passed release build and a directed L0 suite covering exact
flags/OFF priority, all prefixes, lexical boundaries, TLS hit/reentry, both
capacity bounds, overlong paths, cross-thread reconfiguration, generation
exhaustion, and a self-created fork child with explicit reconfiguration. Logs
are in target/port-g1. No application benchmark or speedup is claimed; the prior
Track P measurements did not establish a significant path mapping bottleneck.
