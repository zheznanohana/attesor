# Stage 3 实施与验证报告

起点：`76236d4`，工作树 `/Users/USER/attesor-s3`，分支 `stage3`。未推送。

## A3：块内 dead V stores

修改：`crates/attesor/src/jit/emit.rs` 的 `v_store_dead_at`、`emit_block`、
`VCache`、`E::{store_v,vc_alloc,vfill,access,vfin,simd_fp,simd_more}`；
`crates/attesor/src/aot.rs` 的 translator version 从 48 到 49。

- 前向证明止于本块的公开标签、最大指令数边界、退出、未知/partial writer、helper/fallback。
- 已审计的 scalar binary、FSQRT、FMADD 用缓存和 alias 快照读取操作数；冷路径捕获同一快照。
  同时修正 binary 的 `rm == rd` 缓存读取，不能在目的寄存器失效后从陈旧内存取源。
- 完整 SIMD/GPR 普通 load/store 仅在模板确定原生实现时可跨越；保留 reserved 和 duplicate-GPR-pair
  fallback 屏障。SIMD store/pair/lane store 从 Cpu 读，不是假定的缓存消费者。
- 识别完整 scalar/load 覆盖以及独立输入的 vector logic、ADD/SUB 覆盖；vector 输入别名、
  BSL/BIT/BIF 旧目的值读取、lane/structure/by-element 等未审计消费者保持保守。
- 统一 `store_v` 覆盖已有 scalar binary/FSQRT/FMADD、S/D/Q load 和 `vfin` 完整结果；
  其他模板继续写穿，不为了覆盖率放宽证明。寄存器独占值强制缓存，dirty 淘汰先写回。
- 不跨块/回边保持 dirty 值，不需要新出口 flush 协议。Loopra 和 AOT Plain units 共用此实现。
  `ATTESOR_JIT_OFF=vdead` 关闭；关闭 vcache/scalaralias、有 disabled classes 或 commpage helper 时也不省略。

### 指令计数

`ATTESOR_DUMP_BYTES=8192 --jit-dump`，hex 包成 `.byte` 后用
`clang -c -arch x86_64`、`objdump -d` 解码。统计 FPCR fast edge 到块终结转移的热路径，
不计冷代码/常量池，也不把 FPCR guard 的异常出口算作热块终点。
原始 hex、汇编、脚本在 `target/stage3/`。

| 来自 | 起始 commit | 最终 A3 | 同一最终二进制 OFF=vdead |
|---|---:|---:|---:|
| FPCOLD 指定 nbody words / kernels.c | 893 B / 172 条 | 804 B / 163 条 | 885 B / 172 条 |
| bench.c matmul inner loop | 220 B / 39 条 | 220 B / 39 条 | 220 B / 39 条 |

nbody 纯 dead-store 开关消掉 9 条 store（81 B）；另外 8 B 来自 scalar alias 读取修正。
matmul 块内只有一次累加目的写，结果必须在块出口可见，故不能省略。
nbody 按 FPCOLD 原文保留末尾 `17ffffe5`；其原始目标在给定 words 之前，不擅自改成自环。
matmul words：`fc6d6a61 fd4001c2 1f420020 910021ad 912581ce f12581bf 54ffff41`，
由本机 `clang -O2 -arch arm64 bench/bench.c` 的 `0x100000eb4..0x100000ecc` 提取。

## B：cheap host call/return

修改：`emit.rs` 的 `Unit::new`、`callret_enabled`、`E::{branch_sys,host_call}`；
`jit/mod.rs` 的 enter trampoline；`core/src/cpu.rs` 的 `entry_epoch` 和 offset；
`jit/selftest.rs` 的 `call_program`、`CallsCode`、`run_selftest_calls`、入口路由及两项单测；
translator version 从 49 到 50。

- BL/BLR 先 push guest pc+4，再 CALL；帧布局 `[host PC, expected guest PC]`，每帧 16 B。
- RET 先确认存在帧，比较 `rax` 和 `[rsp+8]`，匹配才 RET。失配以 16 B 步长寻找旧帧；
  没有匹配则通过既有 indirect 路径执行真实 guest target。上限仍为 256 帧。
  深度溢出调用走 plain 路径；相同 guest return PC 可安全折叠等价 host continuation。
- continuation pop guest PC，检查 epoch 后直接跳 unit 内下一块，不写/读 RET_TARGET，也不每调用压 epoch。
  caches/TBI mask/fused host flags 跨 guest call 清除；不形成包含 BL/BLR 的 loopra region。
- 保留 opt-in `ATTESOR_JIT_ON=callret`，OFF 优先于 ON。是否默认开启由下方实际性能数据决定。

### 失效与其他线程：安全边界

1. `jit_enter` 只采样一次全局 EPOCH 到本 Cpu 的 `entry_epoch`。每次 dispatcher 出口，
   既有 trampoline 都用 `HOST_SP` 丢弃全部 host frames；下次入口只可能创建新一代帧。
2. 单靠 dispatcher 观察 epoch 不足以覆盖另一个线程在 open call 期间失效 caller 的情况。
   因而每个返回续体在执行下一条 guest 指令前仍检查全局 EPOCH；失配走无 chain slot、无 IBTC 的
   `st_pc_and_exit(next)`。所有 open frames 随出口丢弃，dispatcher 再清空 thread-local IBTC。
3. RET 可能先落到旧续体的 guard；这依赖**当前运行时已经成立的 append-only host code 生命周期**：
   `CodeCache::alloc` 单调分配，`invalidate_range` 只删索引/撤链，不释放、覆盖或回收 host bytes；
   AOT 也复制到同一 code cache。新翻译不会复用旧 host PC。因此旧 guard 本身始终可执行，
   不存在跳入已释放 host 页的问题。这一保证对所有线程一致。
4. 若 epoch 恰好在 guard 的 load 之后改变，和现有正在执行的 block 一样，线程可能继续旧指令流，
   直到 guest context synchronization。ISB 的不可链 dispatcher 出口仍保留；不承诺异步立即停止所有线程。
   x86 普通对齐 load 具有此处需要的 acquire 顺序；epoch 发布仍使用 SeqCst atomic。
5. 如果以后回收/复用 host code，上述保留旧 guard 的前提不再成立，必须先引入线程静止期/epoch
   reclamation 等协议；不能只靠这次 return guard，或只在 dispatcher reset，就宣称可释放旧代码。

### calls 自测与敏感性

随机前向调用图，BL/BLR、B/BR tail calls、改 LR 返回到无匹配 PC、longjmp-like 返回到外层帧、
270–334 层递归（超过 256 层上限），并有 CRC32 interpreter-helper 屏障。
完整 Plain unit + IBTC entries 使测试实际保留 nested host frames，而非每个 callee 都先退出 dispatcher。
比较所有 snapshot 槽、PC、SVC 数据、guest stack 全缓冲区，无 skipped。
另外验证 open call 中同线程/另一线程改变 epoch 后，旧 guest continuation 不执行；
验证 SVC 丢弃帧后 RET 重新入口。

故意把 RET 的 `cmp rax,[rsp+8]` 改成 `cmp rax,rax`：

```
jit selftest [calls]: ok=6365 mismatch=3635 skipped=0 native=8750 callret=1 deep=1250
```

恢复并重建：

```
jit selftest [calls]: ok=10000 mismatch=0 skipped=0 native=8750 callret=1 deep=1250
```

原始输出：`target/stage3/B-sensitivity.log`、`B-restored-calls.log`；故障修改已撤销。

## 精确测试命令与输出

最终代码执行：

```sh
cargo build --release -p attesor
cargo test --release --workspace -- --test-threads=1
for E in "" loopra vdead callret; do
  for c in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops fpnan tbiblocks calls; do
    ATTESOR_JIT_OFF=$E target/release/attesor --selftest-jit $c 100000 | tail -1
  done
done
```

原始日志捕获使用 `target/stage3/matrix.sh`，开启 `pipefail`、保留每次完整输出并检查 summary，
不是只忽略管道前段失败。为避免后续重建覆盖正在测的二进制，最终矩阵实际调用同 SHA-256 的
`target/stage3/final-attesor`（与 `target/release/attesor` 一致）。
另完整重跑一次 `ATTESOR_JIT_ON=callret` 下的相同四组 OFF 矩阵（OFF=callret 应胜过 ON），
并在 ON 环境重跑 workspace tests。

A3 单独部分在 `target/stage3/baseline-src` 中验证，原始 Cpu/runtime 布局不含 B：

```sh
CARGO_TARGET_DIR=$PWD/target/stage3/baseline-target cargo build --release -p attesor --manifest-path target/stage3/baseline-src/Cargo.toml
CARGO_TARGET_DIR=$PWD/target/stage3/baseline-target cargo test --release --workspace --manifest-path target/stage3/baseline-src/Cargo.toml -- --test-threads=1
bash target/stage3/matrix.sh target/stage3/A3-final-attesor A3-final calls
```

起始 commit 尚无 calls 类。为能在 A3 本身上执行要求的 13 类矩阵，将后来写好的 calls
测试器回移到这个临时验证树，移除其 B Unit 配置覆盖，仅把 A3 的 `callret_enabled`
可见性改成 `pub(super)` 让测试器查询；**不含 B 的 emitter、entry trampoline 或 Cpu 修改**。
B-only epoch/SVC 单测也不回移。适配记录为 `target/stage3/A3-final-harness-notes.txt`。
A3 patch 仍只含 A3，能独立应用到起始 commit。

### Cargo summaries

A3：

```text
test result: ok. 43 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

A3+B：

```text
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

A3+B / ON=callret：

```text
test result: ok. 45 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

### A3 单独 / ON 未设置：52 个 summary 原文

```text
OFF=default jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=default jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=default jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=default jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=default jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=default jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=default jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=default jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=default jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=default jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=default jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=default jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=default jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
OFF=loopra jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=loopra jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=loopra jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=loopra jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=loopra jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=loopra jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=loopra jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=loopra jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=loopra jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=loopra jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=0
OFF=loopra jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=loopra jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=loopra jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
OFF=vdead jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=vdead jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=vdead jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=vdead jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=vdead jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=vdead jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=vdead jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=vdead jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=vdead jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=vdead jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=vdead jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=vdead jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=vdead jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
OFF=callret jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=callret jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=callret jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=callret jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=callret jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=callret jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=callret jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=callret jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=callret jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=callret jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=callret jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=callret jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=callret jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
```

### 最终 A3+B / ON 未设置：52 个 summary 原文

```text
OFF=default jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=default jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=default jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=default jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=default jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=default jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=default jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=default jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=default jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=default jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=default jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=default jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=default jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
OFF=loopra jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=loopra jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=loopra jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=loopra jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=loopra jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=loopra jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=loopra jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=loopra jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=loopra jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=loopra jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=0
OFF=loopra jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=loopra jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=loopra jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
OFF=vdead jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=vdead jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=vdead jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=vdead jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=vdead jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=vdead jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=vdead jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=vdead jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=vdead jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=vdead jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=vdead jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=vdead jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=vdead jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
OFF=callret jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=callret jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=callret jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=callret jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=callret jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=callret jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=callret jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=callret jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=callret jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=callret jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=callret jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=callret jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=callret jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
```

### 最终 A3+B / ON=callret：52 个 summary 原文

```text
OFF=default jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=default jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=default jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=default jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=default jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=default jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=default jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=default jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=default jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=default jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=default jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=default jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=default jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=1 deep=12500
OFF=loopra jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=loopra jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=loopra jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=loopra jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=loopra jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=loopra jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=loopra jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=loopra jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=loopra jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=loopra jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=0
OFF=loopra jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=loopra jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=loopra jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=1 deep=12500
OFF=vdead jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=vdead jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=vdead jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=vdead jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=vdead jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=vdead jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=vdead jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=vdead jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=vdead jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=vdead jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=vdead jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=vdead jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=vdead jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=1 deep=12500
OFF=callret jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
OFF=callret jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
OFF=callret jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
OFF=callret jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
OFF=callret jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
OFF=callret jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
OFF=callret jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
OFF=callret jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
OFF=callret jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
OFF=callret jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
OFF=callret jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
OFF=callret jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
OFF=callret jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
```

额外开关检查（各 10,000 例）：

```text
OFF=nextuse jit selftest [fpblocks]: ok=10000 mismatch=0 skipped=0 insns=64997 fallback_insns=0
OFF=vcache jit selftest [fpblocks]: ok=10000 mismatch=0 skipped=0 insns=64997 fallback_insns=0
OFF=scalaralias jit selftest [fpblocks]: ok=10000 mismatch=0 skipped=0 insns=64997 fallback_insns=0
```

## B 性能：三组交替、--no-aot

构建客体（设置 TMPDIR 到工作树内，避免 sandbox 的系统临时目录限制）：

```sh
export TMPDIR=$PWD/target/stage3/tmp
clang -O2 -arch arm64 bench/bench.c -o target/stage3/bench-arm64
clang -O2 -arch arm64 bench/native-ratio/kernels.c -o target/stage3/kernels-arm64
clang -O2 -arch arm64 bench/native-ratio/callsites.c -o target/stage3/callsites-arm64
bash target/stage3/benchmarks.sh
```

同一 `final-attesor`，OFF 用 `ATTESOR_JIT_ON= ATTESOR_JIT_OFF=callret`，
ON 用 `ATTESOR_JIT_ON=callret ATTESOR_JIT_OFF=`。每组 OFF 后 ON，共三组；
每个客体进程前运行 uptime，所有进程均带 `--no-aot`。测量时自己的 selftests/build 已结束。

| 项目（毫秒） | OFF 1 / 2 / 3 | ON 1 / 2 / 3 |
|---|---|---|
| bench.c fib(36) 标签 | 155 / 27 / 30 | 29 / 31 / 32 |
| bench.c qsort | 454 / 377 / 383 | 400 / 429 / 428 |
| kernels.c qsort | 1150.1 / 1122.0 / 1410.8 | 1219.2 / 1205.1 / 1243.1 |
| callsites.c | 69.6 / 69.3 / 66.8 | 91.4 / 93.9 / 98.0 |

**默认保持关闭**：callsites 三次都更慢（均值 68.57 -> 94.43 ms，约 +37.7%），
bench.c qsort 也没有稳定获益，不满足全部更快的门槛。
第一组 fib=155 ms 是明显离群原始值，未删除；同一 benchmark 实际调用 fib(32)，但原文标签为 fib(36)。
本版本 kernels.c 根本没有 fib 项目，不能编造它的结果；测量了其中现有的 qsort，以及 bench.c 的 fib/qsort。
负载不低，其他项目仅作原始记录，不据此声称广泛性能提升。

### 全部原始计时和 uptime

```text
run=1 callret=off guest=bench
19:45  2 users, load averages: 4.74 5.35 5.79
fib(36)         155 ms
qsort           454 ms
sieve           435 ms
crc32           209 ms
list            307 ms
matmul          120 ms
memops          158 ms
TOTAL          1837 ms
run=1 callret=off guest=kernels
19:45  2 users, load averages: 4.68 5.33 5.78
int-hash      271.7 ms
qsort        1150.1 ms
fp-nbody      147.4 ms
memcpy         32.1 ms
branchy       511.8 ms
run=1 callret=off guest=callsites
19:45  2 users, load averages: 4.68 5.33 5.78
callsites      69.6 ms
run=1 callret=on guest=bench
19:45  2 users, load averages: 4.68 5.33 5.78
fib(36)          29 ms
qsort           400 ms
sieve           385 ms
crc32           209 ms
list            212 ms
matmul           94 ms
memops          163 ms
TOTAL          1492 ms
run=1 callret=on guest=kernels
19:45  2 users, load averages: 4.46 5.27 5.76
int-hash      266.4 ms
qsort        1219.2 ms
fp-nbody      119.4 ms
memcpy         25.8 ms
branchy       485.2 ms
run=1 callret=on guest=callsites
19:45  2 users, load averages: 4.46 5.27 5.76
callsites      91.4 ms
run=2 callret=off guest=bench
19:46  2 users, load averages: 4.46 5.27 5.76
fib(36)          27 ms
qsort           377 ms
sieve           442 ms
crc32           216 ms
list            212 ms
matmul           85 ms
memops          170 ms
TOTAL          1529 ms
run=2 callret=off guest=kernels
19:46  2 users, load averages: 4.35 5.24 5.74
int-hash      253.4 ms
qsort        1122.0 ms
fp-nbody      120.3 ms
memcpy         29.1 ms
branchy       515.0 ms
run=2 callret=off guest=callsites
19:46  2 users, load averages: 4.35 5.24 5.74
callsites      69.3 ms
run=2 callret=on guest=bench
19:46  2 users, load averages: 4.24 5.20 5.73
fib(36)          31 ms
qsort           429 ms
sieve           444 ms
crc32           233 ms
list            223 ms
matmul           91 ms
memops          163 ms
TOTAL          1613 ms
run=2 callret=on guest=kernels
19:46  2 users, load averages: 4.24 5.20 5.73
int-hash      333.1 ms
qsort        1205.1 ms
fp-nbody      112.9 ms
memcpy         27.8 ms
branchy       508.1 ms
run=2 callret=on guest=callsites
19:46  2 users, load averages: 4.06 5.15 5.70
callsites      93.9 ms
run=3 callret=off guest=bench
19:46  2 users, load averages: 4.06 5.15 5.70
fib(36)          30 ms
qsort           383 ms
sieve           450 ms
crc32           216 ms
list            223 ms
matmul           85 ms
memops          146 ms
TOTAL          1533 ms
run=3 callret=off guest=kernels
19:46  2 users, load averages: 4.06 5.15 5.70
int-hash      291.5 ms
qsort        1410.8 ms
fp-nbody      107.0 ms
memcpy         26.3 ms
branchy       587.1 ms
run=3 callret=off guest=callsites
19:46  2 users, load averages: 4.61 5.24 5.74
callsites      66.8 ms
run=3 callret=on guest=bench
19:46  2 users, load averages: 4.61 5.24 5.74
fib(36)          32 ms
qsort           428 ms
sieve           434 ms
crc32           219 ms
list            239 ms
matmul           97 ms
memops          156 ms
TOTAL          1605 ms
run=3 callret=on guest=kernels
19:46  2 users, load averages: 4.56 5.22 5.73
int-hash      289.5 ms
qsort        1243.1 ms
fp-nbody      115.6 ms
memcpy         27.7 ms
branchy       500.3 ms
run=3 callret=on guest=callsites
19:46  2 users, load averages: 4.56 5.22 5.73
callsites      98.0 ms
```

## 不确定项、边界与交付

- 保守模板覆盖说明见 A3；未跨 guest stores、未知 SIMD、helpers 或出口证明 dead stores。
- B 依赖现有 host code 永不回收/复用的生命周期；尚未实现将来释放 host code 所需的跨线程 reclamation 协议。
  跨线程单测改变实际返回 guard 所读的 AtomicU64，验证 guard/退栈行为；不是声称已实现代码释放压力测试。
- 按任务约定未运行需要 sandbox socket bind 的 gdbstub.sh，留给 Claude。
- build/test 有仓库既有 unused/clashing extern 等警告，未顺带修改无关代码。
- git add 无法创建 `/Users/USER/attesor/.git/worktrees/attesor-s3/index.lock`（Operation not permitted），
  因而没有 commit hash，也没有 push。工作树保留全部修改。
- A3 单独补丁：`target/stage3-A3.patch`，只改 emit.rs / aot.rs（48 -> 49），
  在起始 commit 文件副本上通过 git apply --check。
- 分部提交信息：`target/stage3-A3-message.txt`、`target/stage3-B-message.txt`。
- 额外提供 B-only 补丁：`target/stage3-B.patch`，基于 A3 后的树，包含本报告。
- 精确 selftest summaries：`target/stage3/{A3-final,final,final-on}-selftests.log`；
  Cargo summaries 和每一次完整 stdout/stderr、hex/asm、计数/测量/敏感性脚本都在 `target/stage3/`。
