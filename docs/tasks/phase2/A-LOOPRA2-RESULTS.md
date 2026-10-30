# Track A：loopra2 实施报告

## 范围与改动

- 分支 `loopra2`，工作树 `/Users/USER/attesor-p2a`；没有启动 GUI、修改用户应用或使用用户的 Attesor 缓存。所有运行均使用私有 `ATTESOR_CACHE`，未推送。
- `jit/loops.rs`：有界指令 CFG、包含候选 header 的 SCC、基本块边界与支配关系；删除支配回边后必须为 DAG，拒绝不可约内部循环。普查最多 512 条指令 / 8 KiB；分配分析最多 256 条，区域最多 32 块 / 256 条。
- `jit/emit.rs` 的 `emit_block` / `emit_multi_region` / `emit_block_inner`、`goto` / `flags_branch`：仅 `ATTESOR_JIT_ON=loopra2` 启用。整个区域使用同一组 10 个宿主寄存器；内部边直达私有标签。header 的规范入口统一加载，出口统一写回所有已分配槽（不是按代码生成顺序累计的局部 dirty mask）。
- 私有克隆只公开 header；其他公开入口仍单独生成规范代码，因此 header 支配克隆中的所有块，外部侧入口不会携带错误的寄存器状态。临时边界参与现有 flags/vector liveness 证明；不跨块继承 flag fusion。多块区域禁用仅适用于单块回边的延迟 NZCV 写出。
- guest BL/BLR 作为规范区域出口；冷 call arm 不会把其 continuation 整体纳入区域。解释器 helper 仍在区域内同步、调用、重载。保留 phase 1 的 CAS/LSE/SHA scratch 排除、FPCR 检查、cold-path 合约、TBI mask 合约及 fatal-guest-fault 断言。
- JIT 与 AOT 共用该区域发射器；验证模式不形成区域。`jit/mod.rs` 将私有克隆覆盖的 guest 范围纳入失效索引，处理 V8 等运行时生成/改写的代码。`TRANSLATOR_VERSION`：53 → 54（普查）→ 55（初版区域）→ 56（call 出口与预算调整）。ON/OFF 仍使用不同 AOT identity。
- `selftest.rs`：`loops` 增加 diamonds、两臂/头部/中段侧出口、helper/call 单臂、跨边 NZCV、两个真实 latch、嵌套重入、宽寄存器压力及原有内存/SIMD/别名形式；新增 `multi_cases`、`multi_regions`、`guest_calls` 输出。宿主原子计数仅在自测中发射，不进入普通基准或 AOT blob。
- 新增普查报告工具、串行 A/B 驱动、完整检查脚本、跨 16-KiB guest 页的运行时改码检查。AOT 检查增加纯多块 diamond / early-exit / nested fixtures，保留原 7 项。

## 先测量，后实现

实现前先加入显式 `ATTESOR_JIT_ON=loopcensus` 诊断模式：在每块入口（包括私有快入口）发布精确块 PC；现有 1 ms profiler 额外保存完整 PC 样本，stderr 保存有界 CFG。关闭时不改变生成的指令。测量完成并在对话中报告后才加入多块区域。

原始资料：`target/loopra2/measure/`，包含 `runs.json`、三个 stdout/stderr、完整 profiler 及 `*-census.txt`。使用用户提供的 `docs/tasks/phase2/js.js`，Electron 仅以 `ELECTRON_RUN_AS_NODE=1` 启动。命令保存在 `measure/run-census.py`。

| 工作负载 | 被采样循环（至少 1 样本） | 多块循环 | 多块块数分布 | 多块执行样本 | 多块回边数/形状 |
|---|---:|---:|---|---:|---|
| bench.c | 23 | 5 | 2×3、5×1、9×1 | 274 / 全部执行 1901 | B.cond×6、CB×1、B×1 |
| kernels.c | 15 | 4 | 2×1、3×2、11×1 | 147 / 全部执行 2561 | B.cond×4、B×1 |
| headless Node JS | 223 | 194 | 2–122 块，完整分布见 js-census.txt | 2405 / 全部执行 5700 | B.cond×132、CB×43、B×78、TB×33 |

更严格的至少 5 样本阈值：bench 21 个循环 / 3 个多块（272 样本）；kernels 13 / 2（145）；JS 44 / 42（2207）。

JS 的非 Mach-O 地址区（`image=?`，推断主要为 V8 生成代码，未做 V8 code-object 归属验证）：40 个被采样循环，其中 35 个多块。块数分布为 `{2:1,3:5,6:1,7:1,8:4,11:1,12:2,13:1,14:1,19:1,21:1,22:2,24:1,25:1,28:2,30:2,32:2,35:1,36:1,41:1,63:1,65:1,71:1}`；回边 B×31、B.cond×21、TB×2。多块循环占该区域已识别循环样本的 **1808/1824 = 99.1%**，占该地址区全部执行样本 **1808/(3632+245) = 46.6%**，占全 guest 执行样本 31.7%。这部分确定不被 phase 1 的单块区域覆盖。1808 个样本约为名义 1.8 thread-seconds，采样调度不是精确计时。

这些是有界 CFG 的采样估计，不是全部循环或全部 V8 时间；调用者和被调用者不做跨过程 CFG。嵌套样本分配给最小的已识别循环；同地址改码使用最近的翻译元数据，因此时间序列混合是普查的限制。没有样本不代表没有执行。

基线原始 stdout 与 uptime：

### bench

`21:57  2 users, load averages: 11.48 11.72 10.89`；wall=3.214313s，exit=0

```text
fib(36)          32 ms
qsort           497 ms
sieve           543 ms
crc32           272 ms
list            282 ms
matmul          109 ms
memops          204 ms
TOTAL          1939 ms
```

### kernels

`21:57  2 users, load averages: 11.28 11.68 10.88`；wall=3.099150s，exit=0

```text
int-hash      319.5 ms
qsort        1346.4 ms
fp-nbody      133.5 ms
memcpy         36.9 ms
branchy       604.0 ms
```

### js

`21:57  2 users, load averages: 11.02 11.61 10.86`；wall=24.689995s，exit=0

```text
loops        2242 ms 28665
objects      2733 ms -259384288
json         1086 ms 80000
regex         716 ms 400000
strings       282 ms 2025151008
closures     3137 ms 19360
sort         1844 ms 6448448
```

## 最终交替测量（version 56）

命令：`python3 bench/loopra2-measure.py target/release/attesor --dir target/loopra2/ab-final`。
同一个 binary，OFF 为已接受的 phase-1/default，ON 只增加 `loopra2`；均 `--no-aot`、私有缓存，无 profiler/自测计数器。串行 OFF→ON 共 3 轮；不与本任务编译或正确性测试并行。其他 track/用户负载不可控制，以下保留全部原始数字，不丢弃异常值。比值小于 1 表示 ON 更快；使用三组成对比值的中位数，而非中位耗时之比。

注意 bench 输出名 `fib(36)` 的现有源码实际计算 `fib(32)`；没有改动基准源码。C 大基准只输出耗时，数值正确性另由自测、native/interp/JIT/AOT checksum fixtures 验证，不能仅用耗时表宣称等价。JS 7 项结果在所有 6 次运行中逐项一致。

| 工作负载 / 项目 | OFF（3 次，ms） | loopra2（3 次，ms） | 中位成对 ON/OFF |
|---|---|---|---|
| bench/fib(36) | 55.0, 31.0, 36.0 | 27.0, 30.0, 28.0 | 0.778× |
| bench/qsort | 826.0, 386.0, 480.0 | 424.0, 408.0, 396.0 | 0.825× |
| bench/sieve | 928.0, 421.0, 514.0 | 379.0, 495.0, 385.0 | 0.749× |
| bench/crc32 | 274.0, 227.0, 243.0 | 238.0, 262.0, 238.0 | 0.979× |
| bench/list | 260.0, 249.0, 257.0 | 265.0, 383.0, 255.0 | 1.019× |
| bench/matmul | 98.0, 87.0, 88.0 | 110.0, 274.0, 95.0 | 1.122× |
| bench/memops | 152.0, 152.0, 162.0 | 156.0, 281.0, 169.0 | 1.043× |
| bench/TOTAL | 2594.0, 1553.0, 1780.0 | 1599.0, 2134.0, 1567.0 | 0.880× |
| bench/wall | 4291.0, 1734.1, 1956.3 | 1882.3, 2416.2, 1791.3 | 0.916× |
| kernels/int-hash | 280.4, 287.5, 297.7 | 280.0, 359.3, 313.8 | 1.054× |
| kernels/qsort | 1114.7, 1146.3, 1328.7 | 1164.9, 1560.1, 1408.6 | 1.060× |
| kernels/fp-nbody | 112.8, 115.0, 121.3 | 93.4, 111.6, 163.9 | 0.970× |
| kernels/memcpy | 32.5, 38.7, 36.5 | 28.5, 42.1, 49.8 | 1.088× |
| kernels/branchy | 530.5, 756.2, 543.1 | 513.9, 707.2, 674.0 | 0.969× |
| kernels/wall | 2342.2, 2645.7, 2550.0 | 2464.2, 3430.4, 2983.9 | 1.170× |
| js/loops | 678.0, 1357.0, 472.0 | 822.0, 1164.0, 866.0 | 1.212× |
| js/objects | 1022.0, 1064.0, 594.0 | 991.0, 1311.0, 930.0 | 1.232× |
| js/json | 646.0, 238.0, 183.0 | 566.0, 666.0, 707.0 | 2.798× |
| js/regex | 570.0, 244.0, 213.0 | 337.0, 352.0, 309.0 | 1.443× |
| js/strings | 151.0, 113.0, 81.0 | 173.0, 192.0, 203.0 | 1.699× |
| js/closures | 349.0, 272.0, 262.0 | 666.0, 714.0, 568.0 | 2.168× |
| js/sort | 704.0, 560.0, 561.0 | 817.0, 860.0, 794.0 | 1.415× |
| js/wall | 6585.3, 6487.0, 3953.4 | 9232.7, 11149.4, 10812.7 | 1.719× |

### 每次测量的 uptime 与 wall（原始 JSON 同目录 raw.json）

```text
run=1 on=none bench wall=4.291034s; 22:27  2 users, load averages: 5.23 5.84 8.52
run=1 on=none kernels wall=2.342206s; 22:27  2 users, load averages: 6.73 6.14 8.61
run=1 on=none js wall=6.585349s; 22:27  2 users, load averages: 6.73 6.14 8.61
run=1 on=loopra2 bench wall=1.882308s; 22:27  2 users, load averages: 7.10 6.24 8.61
run=1 on=loopra2 kernels wall=2.464151s; 22:27  2 users, load averages: 7.10 6.24 8.61
run=1 on=loopra2 js wall=9.232659s; 22:27  2 users, load averages: 6.85 6.20 8.58
run=2 on=none bench wall=1.734145s; 22:27  2 users, load averages: 6.78 6.20 8.57
run=2 on=none kernels wall=2.645717s; 22:27  2 users, load averages: 6.56 6.16 8.54
run=2 on=none js wall=6.486999s; 22:27  2 users, load averages: 6.56 6.16 8.54
run=2 on=loopra2 bench wall=2.416193s; 22:27  2 users, load averages: 7.07 6.28 8.56
run=2 on=loopra2 kernels wall=3.430441s; 22:27  2 users, load averages: 7.07 6.28 8.56
run=2 on=loopra2 js wall=11.149358s; 22:27  2 users, load averages: 7.14 6.31 8.55
run=3 on=none bench wall=1.956250s; 22:28  2 users, load averages: 6.96 6.30 8.52
run=3 on=none kernels wall=2.549977s; 22:28  2 users, load averages: 6.96 6.30 8.52
run=3 on=none js wall=3.953416s; 22:28  2 users, load averages: 6.89 6.30 8.51
run=3 on=loopra2 bench wall=1.791342s; 22:28  2 users, load averages: 6.74 6.27 8.49
run=3 on=loopra2 kernels wall=2.983871s; 22:28  2 users, load averages: 6.74 6.27 8.49
run=3 on=loopra2 js wall=10.812717s; 22:28  2 users, load averages: 7.24 6.39 8.51
```

**结论：不能建议默认启用。** 代表性循环的指令数下降，但最终 JS wall 的中位成对比值仍为 **1.719×（慢 71.9%）**，kernels wall 为 1.170×；机器噪声较大，不能把 C 子项的改善泛化。初版 version 55 的 JS wall 原始三组为 OFF 5.419/4.066/5.068s、ON 19.805/16.724/17.146s（`target/loopra2/ab/raw.json`）；据此将 guest calls 切为区域出口并收紧预算，version 56 有改善但翻译/克隆成本仍未达接受标准。仍只保留 opt-in，等待 Claude 独立测量与审阅。

### 指令数（不受 wall-time 噪声影响）

`ATTESOR_DUMP_BYTES=2048 ... --jit-dump 3600007c 91000400 14000002 ca010000 f100079c 54ffff61`，关闭/开启 loopra2，另分别 dump 基线各块后用 `.byte`、clang、objdump 反汇编。原始 hex / object / disassembly / 驱动见 `target/loopra2/dumps/` 与 `target/loopra2/dump-count.py`。

对已经 chaining 的持续迭代，排除一次性入口加载、最后一次退出和 dispatcher：基线两个 arm 均 **17 条**；loopra2 的奇数 arm **11 条**、偶数 arm **10 条**。GPR 的循环内 Cpu 写出从每轮 2 次降为 0；4 次 NZCV 字节写出保留。分配的三个 GPR 在入口加载，在出口写回，包括只读槽；这也是保守实现的额外出口成本。静态 dump 的两个分支都计数不能直接当动态迭代指令数。

## 正确性与敏感性

复现：`bash tests/attesor/loopra2-check.sh`（默认每类 100000）。脚本设置私有缓存和 `/private/tmp`，分别以无 flag / `loopra2` 执行 cargo release workspace tests、COMMON 列出的 13 类自测、loops 三种种子、AOT 检查与 SMC 检查。完整日志在 `target/loopra2/check/`。

敏感性：临时仅让多块区域的 `E::writeback` 不写出（phase 1 不受影响），重新构建并执行 `ATTESOR_JIT_ON=loopra2 ... --selftest-jit loops 10000 0x1234`；实际非零退出 134。已撤销故障修改、重新构建，随后运行完整双模式矩阵。保留失败日志 `target/loopra2/sensitivity-v56-broken.log`；没有保留故障开关。

```text
jit selftest [loops]: ok=4569 mismatch=5431 skipped=0 native=7751 loop_regions=15024 seed=0x1234 max_blocks=313 nested_entries=5755 aliases=2187 q_scale16=412 negative_sxtw=2500 multi_cases=8588 multi_regions=13151 guest_calls=624
```

### 完整最终测试输出行

```text
off test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s
off test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
off test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
off test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
off jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
off jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
off jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
off jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
off jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
off jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
off jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
off jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
off jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
off jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=77774 loop_regions=18645 seed=0x1234 max_blocks=3125 nested_entries=56712 aliases=21875 q_scale16=4607 negative_sxtw=25000 multi_cases=85934 multi_regions=0 guest_calls=6252
off jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
off jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
off jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
off jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=77836 loop_regions=18677 seed=0xdeadbeef max_blocks=3125 nested_entries=56177 aliases=21875 q_scale16=4725 negative_sxtw=25000 multi_cases=85934 multi_regions=0 guest_calls=6252
off jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=77794 loop_regions=18810 seed=0xc0ffee max_blocks=3125 nested_entries=55746 aliases=21875 q_scale16=4662 negative_sxtw=25000 multi_cases=85934 multi_regions=0 guest_calls=6252
PASS int-hash: native=interp=cold-JIT=warm-AOT checksum 18597cb650ab5d23; blocks=7 aot_loop_regions=1
PASS int-hash: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS crc32: native=interp=cold-JIT=warm-AOT checksum 00000000f941502b; blocks=15 aot_loop_regions=3
PASS crc32: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS sieve: native=interp=cold-JIT=warm-AOT checksum 0000000000000404; blocks=21 aot_loop_regions=39
PASS sieve: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS branchy: native=interp=cold-JIT=warm-AOT checksum ffffffffffffff64; blocks=13 aot_loop_regions=2
PASS branchy: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS pointer: native=interp=cold-JIT=warm-AOT checksum 0000000000176bcd; blocks=13 aot_loop_regions=2
PASS pointer: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS pre-post: native=interp=cold-JIT=warm-AOT checksum 0000000007ac5100; blocks=17 aot_loop_regions=3
PASS pre-post: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS ldp-stp: native=interp=cold-JIT=warm-AOT checksum 000000000f681980; blocks=17 aot_loop_regions=3
PASS ldp-stp: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS loopra AOT: all 7 workloads and cache identities match; jit_on=none
loopra2 test result: ok. 72 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.38s
loopra2 test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
loopra2 test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
loopra2 test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
loopra2 jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
loopra2 jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
loopra2 jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
loopra2 jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
loopra2 jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
loopra2 jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
loopra2 jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
loopra2 jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
loopra2 jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
loopra2 jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=77478 loop_regions=113128 seed=0x1234 max_blocks=3125 nested_entries=56712 aliases=21875 q_scale16=4607 negative_sxtw=25000 multi_cases=85934 multi_regions=94483 guest_calls=6252
loopra2 jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
loopra2 jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
loopra2 jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
loopra2 jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=77529 loop_regions=113799 seed=0xdeadbeef max_blocks=3125 nested_entries=56177 aliases=21875 q_scale16=4725 negative_sxtw=25000 multi_cases=85934 multi_regions=95122 guest_calls=6252
loopra2 jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=77488 loop_regions=113757 seed=0xc0ffee max_blocks=3125 nested_entries=55746 aliases=21875 q_scale16=4662 negative_sxtw=25000 multi_cases=85934 multi_regions=94947 guest_calls=6252
PASS int-hash: native=interp=cold-JIT=warm-AOT checksum 18597cb650ab5d23; blocks=7 aot_loop_regions=1
PASS int-hash: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS crc32: native=interp=cold-JIT=warm-AOT checksum 00000000f941502b; blocks=15 aot_loop_regions=3
PASS crc32: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS sieve: native=interp=cold-JIT=warm-AOT checksum 0000000000000404; blocks=21 aot_loop_regions=3
PASS sieve: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS branchy: native=interp=cold-JIT=warm-AOT checksum ffffffffffffff64; blocks=13 aot_loop_regions=2
PASS branchy: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS pointer: native=interp=cold-JIT=warm-AOT checksum 0000000000176bcd; blocks=13 aot_loop_regions=2
PASS pointer: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS pre-post: native=interp=cold-JIT=warm-AOT checksum 0000000007ac5100; blocks=17 aot_loop_regions=3
PASS pre-post: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS ldp-stp: native=interp=cold-JIT=warm-AOT checksum 000000000f681980; blocks=17 aot_loop_regions=3
PASS ldp-stp: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS diamond: native=interp=cold-JIT=warm-AOT checksum 000000000000004b; blocks=12 aot_loop_regions=1
PASS diamond: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS diamond: loopra2-only toggle has a distinct AOT identity and matching warm output
PASS early-exit: native=interp=cold-JIT=warm-AOT checksum 0000000012345645; blocks=13 aot_loop_regions=1
PASS early-exit: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS early-exit: loopra2-only toggle has a distinct AOT identity and matching warm output
PASS nested: native=interp=cold-JIT=warm-AOT checksum 000000000000114b; blocks=12 aot_loop_regions=1
PASS nested: loopra=off rejects on-cache, distinct identity, warm output matches, aot_loop_regions=0
PASS nested: loopra2-only toggle has a distinct AOT identity and matching warm output
PASS loopra AOT: all 10 workloads and cache identities match; jit_on=loopra2
on=none mode=--interp loopra2 SMC PASS checksum=20000
on=none mode=--no-aot loopra2 SMC PASS checksum=20000
on=loopra2 mode=--interp loopra2 SMC PASS checksum=20000
on=loopra2 mode=--no-aot loopra2 SMC PASS checksum=20000
```

额外组合验证（每项 loops 10000，seed=0x1234）：

```text
loopra2,loopcensus jit selftest [loops]: ok=10000 mismatch=0 skipped=0 native=7751 loop_regions=11437 seed=0x1234 max_blocks=313 nested_entries=5755 aliases=2187 q_scale16=412 negative_sxtw=2500 multi_cases=8588 multi_regions=9564 guest_calls=624
loopra2,callret jit selftest [loops]: ok=10000 mismatch=0 skipped=0 native=7751 loop_regions=11437 seed=0x1234 max_blocks=313 nested_entries=5755 aliases=2187 q_scale16=412 negative_sxtw=2500 multi_cases=8588 multi_regions=9564 guest_calls=624
ON=loopra2 OFF=loopra2 jit selftest [loops]: ok=10000 mismatch=0 skipped=0 native=7787 loop_regions=1873 seed=0x1234 max_blocks=313 nested_entries=5755 aliases=2187 q_scale16=412 negative_sxtw=2500 multi_cases=8588 multi_regions=0 guest_calls=624
```

SMC fixture 将 header 放在第一个 16-KiB guest 页、body 放在第二页且超出旧 header+MAX_BLOCK_INSNS 范围；先执行 1000 次，再只失效 body 的 4 个字节，改码后再执行 1000 次。解释器与 JIT、关闭/开启 flag 均应得到 checksum=20000；这也覆盖非文件映射的运行时代码。

### headless Node 加密 / TLS

使用提供的 `docs/tasks/phase2/t.js`，命令与 JS 基准相同但替换脚本。关闭/开启分别保存到 `target/loopra2/tls-final/`。

`off`：

```text
sha256 ca6073392ee71dbd
sha512 fcf47d64f5ceadd9
gcm 89788ae8c19a5009 tag de047988105bcc38
ecdsa verify true
ecdh true
string 13 e5b7a6e4b88ae8a792e9a1b6e6a08f
https error ENOTFOUND
```
`loopra2`：

```text
sha256 ca6073392ee71dbd
sha512 fcf47d64f5ceadd9
gcm 89788ae8c19a5009 tag de047988105bcc38
ecdsa verify true
ecdh true
string 13 e5b7a6e4b88ae8a792e9a1b6e6a08f
https error ENOTFOUND
```

两侧 SHA256、SHA512、AES-GCM/tag、ECDSA、ECDH、Unicode 输出一致。**DNS 返回 ENOTFOUND，没有验证到 HTTPS 401 或 TLS 握手成功**；需要在网络可用环境补跑，不能把此项报为联网通过。两侧均有相同的 Electron task_name_for_pid 权限诊断（stderr 保留），进程 exit=0。

## 未覆盖与风险

- 默认关闭；JS 整体仍明显退化。需要进一步降低逐单位 CFG/支配分析及克隆成本、改善热度/收益选择，不能仅凭短循环指令数启用默认。
- 区域有界，超过预算、不可约 CFG、CAS/LSE/SHA scratch 等保守退出；必经 guest call 的循环不保持寄存器状态跨 call，只有无 call 的循环路径形成区域。cold call arm 规范退出后可重新进入 header。
- dirty mask 为整区域保守全集，出口可能写回只读槽。没有跨基本块保留 SIMD cache 或未经证明的 NZCV fusion。
- 沿用现有不可恢复 guest fault/协作式代码失效语义；没有添加 guest fault handler 支持。未运行 GUI 实际应用。
- 普查不能严格区分匿名 V8 code object / 其他非 Mach-O 代码，也不是精确时间归因；原始数据及限制已明确列出。
- TLS 401 待网络可用后补验。没有做远程请求重试或更改用户提供的 TLS 脚本。

## 提交与补丁

工作树的 `.git` 元数据为只读，未写入共享 Git 元数据、未提交、未推送，因而没有 commit hash。按 COMMON 的 fallback 保留两步提交说明与补丁：

- `target/loopra2-measurement-commit-message.txt`
- `target/loopra2-implementation-commit-message.txt`
- `target/loopra2-measurement.patch`
- `target/loopra2-implementation.patch`

两个补丁从原始 HEAD `a7682a1` 顺序应用；验证记录 `target/loopra2/patch-check.log`。完整执行证据位于本工作树忽略的 `target/loopra2/`，报告本身保留在 repository 文档中。
