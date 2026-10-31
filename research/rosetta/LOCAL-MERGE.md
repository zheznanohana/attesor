> Superseded default policy: the owner subsequently requested default-on.
> See [LOCAL-DEFAULTS106.md](LOCAL-DEFAULTS106.md). The v105 installation below
> is a historical checkpoint, not the current default selection.

# Local usable merge / 2026-10-31

Merge the research source into the owner's local v2 branch. General `attesor`
keeps the existing conservative default flags; do not silently enable research
optimizations for every application. The CPU translator remains codegen105.
No further optimizer change or new performance claim is made by this packaging.

`scripts/attesor-fast`, installed beside `attesor`, is an explicit JIT-only
entrypoint for the measured research profile through provisional recleaf101 and
inlinewide105. Rejected inlinegp/bytestream/flag/address/batch modifiers stay OFF.
An explicitly set ATTESOR_JIT_ON, including empty, still replaces that profile;
ATTESOR_JIT_OFF still wins per feature. ATTESOR_BIN can select a separate binary.

```
attesor ./program-arm64           # existing general default
attesor-fast ./program-arm64      # explicit experimental local fast profile
```

This profile has focused correctness tests and original-fixture measurements,
not full application/AOT/concurrency acceptance. Sorting4M improved10.5% median
but is58.5% of native; companion FP/branch observations are not all improvements.
Do not advertise all-item >=60% completion or general stability. Keep the old
installed executable and original branch checkpoint for rollback. Replace the
binary atomically so running processes keep their existing mapped executable.
The local CPU-only installation does NOT rebuild GPU/native bridges, change
launch agents, shell startup files, wallpaper, security, sleep or system files.
