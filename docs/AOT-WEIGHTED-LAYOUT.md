# Explicit sample-guided offline layout (integ1)

Default off. Enable with `ATTESOR_JIT_ON=offline-layout-profile` and
`ATTESOR_AOT_LAYOUT_PROFILE=/absolute/path/to/samples.txt` when running
`attesor --prewarm /path/to/image` (or the existing checkpoint builder).
`ATTESOR_JIT_OFF=offline-layout-profile` or `offline-layout` wins.
This port uses the existing fixed-address MCLAOT05 whole-file builder, not the
older incremental chunks/offline-cache infrastructure. No new record format.

UTF-8 example (replace KEY and size with the actual image identity):

```
Attesor AOT layout samples v1
image KEY 8192
pc 0 12
pc 0x10 99
end
```

Only exact already-discovered public entries are reordered: descending sample
weight, then observed-only entries, then static cold entries; ties use guest PC.
Hot/cold boundaries split 1024-entry assembler units. Without a matching profile
the original sorted-PC/4096-entry placement is unchanged. Sample rows never
create entries or round interior PCs. Input is image-relative, key/size checked.

A bounded 4 MiB / 100000 PC / 256 image snapshot is read once per process;
symlinks, nonregular files, invalid UTF-8, duplicates, missing end, unaligned or
out-of-bounds PCs, zero/overflowing weights reject the entire configuration.
Content hash and algorithm tag enter generated-code/AOT identity, in addition
to existing ON/OFF/version/build UUID identity. Changed contents at the same
path cannot reuse an old identity. Use a private cache root for experiments.

The existing PC set and aggregate sampler do not supply exact-entry frequencies.
Counts must come from a real external sample producer; no performance benefit
is claimed. Existing blobs are replaced through the normal whole-file policy,
not rearranged in place. L0 ordering/parser/chunk tests and a no-op-layout
sensitivity control cover this port; see target/port-g1 for actual run logs.
