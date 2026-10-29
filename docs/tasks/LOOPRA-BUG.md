# Task brief: find the loopra miscompile (urgent correctness bug)

Reviewer: Claude. Implementer: Codex. Work only in `~/attesor-dbg` (branch `loopra-dbg`). Never touch
`~/attesor`, `~/bin`, user apps, caches or processes you did not start. If git is blocked, leave the work in
the tree and write the message to `target/loopra-dbg-commit-message.txt`. Do not push.

## Symptom (real apps, since hot-loop register allocation became default-on)
- DeepSeek Harness (Electron 44 / Node 24 / BoringSSL): every HTTPS request fails with `EPROTO` (TLS).
  Reproduced headless: `ELECTRON_RUN_AS_NODE=1 attesor --no-aot "<DSH exe>" t.js` where t.js does
  `https.get('https://api.deepseek.com/models')` → `EPROTO`; with `ATTESOR_JIT_OFF=loopra` → HTTP 401 (OK).
  `ATTESOR_JIT_OFF=vdead` alone does not help. SHA-256/512, AES-256-GCM, ECDSA, ECDH results are all
  correct, so the bad code is elsewhere on the TLS path (or memory gets corrupted).
- Menu-bar text is garbled in both DeepSeek Harness and VS Code (AppKit/CoreText code), probably the same bug.
- The existing `--selftest-jit loops` (100k random loops) passes. So some instruction form, register state
  or entry/exit situation is not covered.
- Claude is bisecting by loop address in parallel; do not wait for that.

## Do
1. Review `LoopRa` / region code in `crates/attesor/src/jit/emit.rs` (region templates `src`/`dst`,
   `region_binary`, `region_dp_imm`, loads/stores incl. register-offset with UXTW/SXTW/LSL extends,
   pre/post-index, pairs, 32-bit (W) operations and compares when the host register's upper 32 bits hold
   stale data, sign-extending loads (LDRSW/LDRSB/LDRSH), CSEL/CSINC 32-bit, CBZ/TBZ on W registers,
   exit write-back, exit flag materialization (`loop_exit_flags`), TBI mask in r11 across the backedge and
   after helpers/cold paths, commpage-fix path inside regions, stack temporaries, the AOT/Plain path).
   Write down every suspicious spot.
2. Strengthen the `loops` generator so it would catch those: every operand form above; initial register
   values with nonzero upper halves for registers used as W; negative 32-bit indices for SXTW; aliasing
   (dst == index/base register); flags read after the loop exit; nested loop entry many times with few
   iterations; loop bodies at the maximum block length; registers 16-30; and loads/stores of tagged-looking
   pointers if TBI is in play. Run millions of cases (use several seeds, add a seed option).
3. When a case fails, minimize it, fix the translator, add the minimized case as a unit test, and show the
   sensitivity (the test fails without the fix).
4. Run the full list: cargo test; all self-test classes with and without `ATTESOR_JIT_OFF=loopra`, mismatch=0.

## Report
Suspicious spots found, failing cases and their minimization, the fix, test lines, commit message path.
