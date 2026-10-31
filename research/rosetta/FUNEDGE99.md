# Experimental codegen99: conditional packed CMP flags (not selected)

Owner-authored compiler work, not recovered Apple code. Opt-in `funedge`, OFF
wins; production executable untouched. Existing closed recursive GP `funra`
scopes with a dedicated packed flag register only. No algorithm/stack rewrite.

Captured native prefix proof must overwrite NZCV before any reader, helper,
system, unsupported memory/template or control transfer. An exact direct self
call counts as killing incoming NZCV ONLY with that prefix witness. Its NEW
callee flags and original return-epoch guard are still required. Bounded
per-successor paths reject missing words, cycles, readers, returns, external
calls and uncertain templates. All proof words lie in the owned function source
snapshot/invalidation range; actual emitted members are checked native/no-helper
(except original self host calls). A CMP adjacent to B.cond may skip its four
packed-flag instructions on a dead private edge; the live edge packs BEFORE goto,
so target SP guards can freely clobber host EFLAGS. Original RAS limit, native
markers, LR scanning, epoch checks and canonical exits are unchanged.

Nine focused release tests pass: eight funra tests plus existing cross-thread
open-call epoch regression. They include175,616 all-condition full-state/stack
cases (both live-edge directions, W/X, tags, Plain/Patchable, funstack OFF/ON),
512 original fib full-state/full-stack cases,16 deep300 or epoch cases. The epoch
probe explicitly requires callee NEW NZCV=6 on invalidation, not incoming13 or
caller2. Existing related guards/source checks also pass. Not a full suite or
real-AOT/application acceptance run.

One unchanged original bench screen showed -7.36% fib time, warranting ONLY two
extra rounds. Results:

| round | native ms | funstack control ms | +funedge ms |
|---|---:|---:|---:|
|1|5.111|13.087|12.124|
|2|5.227|14.273|13.764|
|3|5.818|14.599|14.989|

Median14.273->13.764ms (-3.57%),37.98% native. Paired deltas[-7.36,-3.57,+2.67]%:
NOT a stable substantial improvement. **Keep funedge OFF**, do not repeat again
or sum gains across earlier batches. Selected experimental profile stays through
codegen98 funstack. Release SHA256:
`12c568d466e2262e433913ce8b4528963a4ded66418ba4788c277d95d2d4711f`.
All7 CHECK lines match each execution; full common-config>=60 goal incomplete.

Important new source evidence: logs hit ONLY header CMP1000011b0 (two stack
variants), not tail CMP1000011dc. The tail is CMP; MOV X20,X0; B.HI, not adjacent
CMP+B.HI. Current emitter discards fusion metadata at MOV although this fixed-map
MOV lowers to flag-preserving moves. Packed B.HI otherwise costs TEST packedReg,
SETZ AL, then TEST AL and JCC. A separately gated, audited native-EFLAGS carry
through exact MOV is a better next hypothesis than more header-packing retries.
