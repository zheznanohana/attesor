# Task brief: pre-slid shared-cache data (stage 2c)

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-dsc` (branch `dscslide`). Never touch
`~/attesor`, `~/bin`, user apps, `~/Library/Caches/Attesor`, or processes you did not start (use a private
`ATTESOR_CACHE` dir for tests). If git is blocked, leave work in the tree, message in
`target/dscslide-commit-message.txt`. Do not push.

## Evidence
Profiler: `shared_region_map_and_slide_2_np` costs ~800 ms of emulation per guest process start (one call
per process; DeepSeek Harness starts ~6 processes, VS Code more, and every spawned helper pays it).
`cache::map_and_slide_2` (`crates/attesor/src/cache.rs`) maps the arm64 dyld shared cache at the fixed
`CACHE_BASE`, then `apply_slide_info` rewrites every pointer in the slid data mappings, dirtying all of
those pages (copy-on-write) eagerly. Because `CACHE_BASE` is fixed, the slide — and therefore the result —
is identical in every process and every run.

## Do
1. Measure: total bytes of mappings with `slide_size != 0`, time spent in the mmap pass vs
   `apply_slide_info`, pages dirtied. Report them.
2. Cache the slid result: the first process that applies the slides writes the slid bytes of those mappings
   to a file in the translation cache dir (`aot::cache_dir()` / `ATTESOR_CACHE`), keyed by the shared cache
   UUID, the slide, the mapping table (addresses/sizes/file offsets/protections) and a format version.
   Later processes map that file (MAP_PRIVATE | MAP_FIXED, same protections as today) instead of mapping the
   original file region and applying slides. Write atomically (tmp + rename, flock like the AOT files);
   validate size and a header/checksum before use; fall back to today's path on any mismatch or error.
   Concurrency: several processes may start at once; only one should write, others either wait briefly or
   use the old path.
3. Disk budget: report the file size. If it is larger than ~400 MB, consider storing only pages that slides
   actually change and mapping the rest from the original file, or explain why not.
4. Housekeeping: the AOT `tidy_cache` must treat the new file like the others (stale versions removed,
   30-day rule, touch on use).
5. Tests: a unit test of the cache key/validation/fallback logic; an end-to-end check that a guest program
   (e.g. a Foundation/CoreFoundation-using arm64 test program) gives identical output and identical bytes in
   a sample of slid pages with and without the cache, cold and warm; time per process start before/after
   (measure `attesor <small arm64 program>` wall time, 5 runs each, with a private cache dir).
6. Full self-test list and `cargo test --release --workspace` pass.
## Report
Measurements, design, file size, before/after start time, test lines, commit message path.
