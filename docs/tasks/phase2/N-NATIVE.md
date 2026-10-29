# Track N — native host replacements for system libraries (#26)
Branch `nativelib`, worktree `~/attesor-p2n`. Read COMMON.md and docs/NATIVE-LIBRARIES.md (existing opt-in zlib
bridge, `native/`, libattesornative, `ATTESOR_NATIVE_LIBS`). Do not edit the translator core.
1. Measure first: build small arm64 programs that use the big system libraries the way native Mac apps do
   (ImageIO decode/encode of PNG/JPEG/HEIC, vImage convolution/scale, vDSP FFT/dot products, BNNS if trivial,
   libcompression LZFSE/LZ4/zlib, CoreFoundation dictionaries/strings, CommonCrypto); time them under attesor
   vs the same programs built for x86-64. Report the ratio per API.
2. Pick the pure-C, data-in/data-out APIs with the worst ratios; route them to the host's x86-64 implementation
   (like the zlib bridge): argument marshalling only where layouts differ, no ObjC objects, no callbacks into
   guest code, thread-safe. Opt-in per library (`ATTESOR_NATIVE_LIBS=...`).
3. Verify: identical outputs (bytes) for every API with/without the bridge, error paths, concurrency, and the
   speedups. Keep a list of APIs rejected and why.
