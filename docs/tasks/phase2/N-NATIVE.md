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

## Round 2 (Claude) — acceptance policy and scope
Round 1 committed (b415f5b survey, cd54693 vImage convolution, 24-30x). Your survey shows the biggest
translation losses are in hand-tuned NEON libraries: vDSP FFT 231x, vImage scale 68x, JPEG 27-34x,
PNG 16-20x, LZ4 10-13x, zlib decode 11x, CoreFoundation 7x, LZFSE 6x, AES-CBC 3.3x.
Acceptance is too strict today: the host result *is* what the x86-64 build of the same app gets on this
machine, i.e. correct macOS behaviour here. New rules:
- Compression encoders: output may differ byte-wise; accept if it is valid for the format and decodes (with
  both host and guest decoders) to exactly the input. Decoders: exact bytes.
- Floating-point APIs (vDSP, vImage float/scale): accept if within the API's documented precision, and
  report max abs/relative error vs the guest implementation on varied inputs (incl. denormals, NaN/Inf where
  defined). Integer image ops: exact unless the API documents otherwise.
- Image codecs (ImageIO PNG/JPEG decode/encode): decode → identical pixels for PNG, within JPEG decoder
  tolerance for JPEG; encode → decodable, same dimensions/format, visually equivalent (PSNR report).
Scope, in order of ratio x likelihood of use: vDSP FFT (+ common vDSP ops), vImage scale/convert,
libcompression LZ4/LZFSE/zlib (encode+decode), ImageIO decode/encode via data-in/data-out paths
(CF objects only as plain data; no guest callbacks), CommonCrypto AES (host AES-NI), CoreFoundation only if a
pure-data hot path exists. Each library opt-in (`ATTESOR_NATIVE_LIBS=...`), then report which ones are safe to
enable by default. QA tiered (COMMON.md).
