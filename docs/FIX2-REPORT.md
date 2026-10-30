# FIX2：rel32chain / sharedtrans

状态：主要退化已修复；**逐 kernel 不慢于 OFF 的严格验收尚未通过**。
不能把下表中的残余回退归为已证明的“固有开销”，没有丢弃任何 token。

## 范围与证据

两项仍为精确 token、默认 OFF；`ATTESOR_JIT_OFF` 优先。没有丢弃功能，未引入已发布指令热补丁。

原始副本为 `../attesor-fix2-orig`（初始 `cp -R` 副本）；
`/private/tmp/attesor-fix2-evidence/original` 与其排除 `.git` / `target` 后逐文件一致。
进入本轮时工作树已有未完成的 FIX2 修改。本轮在编辑前用 `cp -R` 另存全部源文件和文档到
`/private/tmp/attesor-fix2-original`（不复制 .git / target）；该快照**不是**性能表的 before。
最终 before 从真正原始副本重新 release 编译，after 从工作树 release 编译。不使用 git。

运行平台：x86_64 Darwin 25.6.0 / macOS 26.7.2 (25G332)，rustc 1.99.1。
CPU 型号查询被 sandbox 拒绝，不猜测。生产路径 `shm_open` 实测 EPERM；因此分别报告普通生产路径与 test-only file-backed 协议路径，绝不把后者称作真实 POSIX shm 验收。

## 根因与修改

### #20 rel32chain

原实现有两个明确的永久禁链条件：`run` 的 EXIT_CONTINUE 不调用 `chain_slot`，而 `chain_slot` 自身也直接拒绝 rel32chain。
发布时只可把新 unit 链向已有旧 target；先翻译 caller、后翻译 callee 的调用永远留在 dispatcher fallback。
每次退出都回 Rust dispatcher，并在下次 entry 执行 CPUID。若另外启用 callret，退出还会丢掉 native guest-call 栈；题给单 token 测量中的 callret=0，不把 native frame 丢失说成该测量的主因。不是重复热 patch、mprotect 或 icache flush。

独立诊断的 k-arm64 原始运行记录 `target/fix2/counters-before-rel32chain.log`：
270,840,737 次 entry serialization，270,832,605 次 slot fallback，**0 次 slot link**，599 条发布前 rel32 边。
这些计数来自独立开启 `ATTESOR_FIX2_COUNTERS=1` 的诊断运行，不混入最终无计数器时间表。

修复：

1. 恢复晚到目标的 mutable data-slot 链接，使用 atomic Release store；保留原 incoming 撤链和 cold/AOT 所有权检查。
2. 用 graph read lease 覆盖 CPUID 后的**完整** generated execution，包括 helper 与 native guest-call frame。
   新 slot 边在 TRANSLATE 之前 `try_write` graph lease；有执行者就保留 fallback，绝不等待无限 guest。
   发布时图静止，下次执行先拿 read lease 再 CPUID；持 read lease 的运行不能获得通向新代码的边。
   失效只缩图，不拿 exclusive lease，避免执行中的 helper 自锁。
3. canonical body 与永久 alive/epoch guard 分离：dispatcher、IBTC、mutable slot 用 canonical body；
   只有不可变 E9 指向 guard。alive cell 和旧代码永不回收；epoch/dead 拒绝恢复 PC、清 EXIT_DATA。
4. guard 放在 unit 的冷附录，不放热入口前。逐字节定向测试证明同一 unit 的 body、loop、stub、slot、constant pool 与无 guard 版本相同；
   整体 unit 长度仍增加，不能声称后续 unit 的全局地址布局完全不变。
5. guest fork 的 child hook 替换永久 graph-lock 指针，避免继承已消失线程的 read lease；旧锁不释放。

这不是把 RwLock Acquire 当成取指屏障：实际取指屏障仍是 CPUID。没有热写 E9、没有重复重新 patch、没有 W^X 切换。
更完整的发布/撤销推导见 `REL32-CHAIN.md`。原工程对非协作并发 SMC 的保证没有被扩大。

### #28 sharedtrans

两条独立机制：

- 每次翻译重新解析环境、构造 identity、逐指令 Mach 读源代码，然后对合格候选重新 open/map/close 共享池。
  即使连接因 EPERM 永远失败也重复尝试。诊断原始 k：3,313 次 prepare、5,086 次源快照读、230 次 mapping open、0 次 adoption。
  这不是每个 dispatcher miss 都拿共享锁，协议也没有 flock；主要是翻译阶段重复工作。
- 连接成功时，Plain wire 翻译没有 patchable exit slot，热直接边每次都回 Rust dispatcher。
  test-only file-backed 原始 k 诊断：215 个本地 Plain、30,006,746 次 dispatcher entries，qsort 3982.3 ms。
  即使只修连接缓存也不会消除这个问题。

修复：冻结启用策略；`OnceLock<Option<Mapping>>` 同时缓存连接成功和失败；先查 available 再做 identity/source 工作；
源快照按不跨 4 KiB 边界的批次读取，仍要求 Mach 返回完整长度、严格 guest/host 策略和 adoption 前源字节再验证。
Wire bytes 仍只含无指针的 Plain 代码；本地 copy 的 dispatcher jump 被引向**本进程**的一个对齐 mutable slot，
fallback 填本地 EXIT_DATA。slot 纳入已有 incoming/slot_cont/source-pages/epoch 失效协议。
共享 mapping 不可执行、wire bytes 不写本地指针，不把无 guard Plain entry 暴露给不可变 rel32 边。

共享槽仍有固定 512 项容量和线性扫描，没有引入新的 per-dispatch lookup/lock。
翻译格式版本 66 → 67，防止旧身份/旧翻译与当前输出混用。

## 测量方法

`tools/fix2-measure.py` 串行执行，清除继承的全部 `ATTESOR_*` 环境变量，普通生产测量仅设置所需 ON token，均 `--no-aot`。
每个 guest、每轮同时交错 before/after × OFF/rel32chain/sharedtrans，轮次旋转并反转顺序；最终生产表为 **5 轮**，不是选择最快一轮。
测量期间不运行构建或正确性测试。记录原始每行毫秒、exit code、命令、wall time、stdout/stderr。

```sh
cargo build --release --offline --manifest-path /private/tmp/attesor-fix2-evidence/original/Cargo.toml \
  --target-dir target/fix2/pristine-target
cargo build --release
python3 tools/fix2-measure.py \
  --before target/fix2/final/attesor-before --after target/fix2/final/attesor-after \
  --guests target/fix2 --out target/fix2/final/production.json --runs 5
```

Guest 是题给目录中二进制的原样 copy，不重新编译 fixture：

- k-arm64 SHA256 `af8eff95d043b5e49fc5648675b9c2e59dcadaa9188ea25af48252faeca88484`
- b-arm64 SHA256 `aebfb528168131f573443af5670cca4b8411774435e1f177cd93de518b017b99`

无 checksum 的原始 benchmark 只能作性能证据；语义依靠下面差分/定向测试，不用 timing-only 输出冒充数值一致性证明。


## 交错测量：第一阶段（5 轮）

本阶段 after 为 guard 冷附录版本；原始 JSON 为 `target/fix2/final/production.json`，
每个组合 5 次，共 60 次完整 guest 执行，全部 rc=0。单位 ms，中位数，不删离群值。
`tools/fix2-summary.py` 可重算下表并输出每个 kernel 的全部原始样本。

| guest / kernel | before OFF | before rel32 | before shared | after OFF | after rel32 | after shared | rel32/OFF | shared/OFF |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| k-arm64 / int-hash | 455.8 | 430.6 | 430.6 | 450.2 | 438.4 | 453.7 | 0.974 | 1.008 |
| k-arm64 / qsort | 1207.2 | 19466.2 | 1204.9 | 1246.4 | 1232.3 | 1206.3 | 0.989 | 0.968 |
| k-arm64 / fp-nbody | 125.8 | 792.9 | 125.7 | 128.3 | 126.0 | 125.6 | 0.982 | 0.979 |
| k-arm64 / memcpy | 35.2 | 36.3 | 33.4 | 33.2 | 33.0 | 33.8 | 0.994 | 1.018 |
| k-arm64 / branchy | 1192.4 | 1170.1 | 1228.5 | 1237.7 | 1177.2 | 1188.0 | 0.951 | 0.960 |
| b-arm64 / fib(36) | 32.0 | 367.0 | 31.0 | 29.0 | 33.0 | 31.0 | 1.138 | 1.069 |
| b-arm64 / qsort | 416.0 | 6686.0 | 409.0 | 411.0 | 433.0 | 409.0 | 1.054 | 0.995 |
| b-arm64 / sieve | 422.0 | 2724.0 | 452.0 | 508.0 | 452.0 | 450.0 | 0.890 | 0.886 |
| b-arm64 / crc32 | 290.0 | 284.0 | 279.0 | 281.0 | 284.0 | 279.0 | 1.011 | 0.993 |
| b-arm64 / list | 270.0 | 269.0 | 268.0 | 269.0 | 272.0 | 261.0 | 1.011 | 0.970 |
| b-arm64 / matmul | 97.0 | 114.0 | 100.0 | 103.0 | 104.0 | 100.0 | 1.010 | 0.971 |
| b-arm64 / memops | 290.0 | 297.0 | 300.0 | 290.0 | 310.0 | 294.0 | 1.069 | 1.014 |
| b-arm64 / TOTAL | 1814.0 | 10742.0 | 1819.0 | 1860.0 | 1894.0 | 1837.0 | 1.018 | 0.988 |


比值大于 1 就是该表测得更慢，不将其改名为“相等”。尤其 b/fib 的 rel32chain
33/29=1.138，b/qsort 433/411=1.054，不能宣称满足用户的全 kernel 要求。
环境也出现明显波动（例如 after OFF 的 k/branchy 有 2503.1 ms），因此表格不是
对每个小百分比回退的因果证明；没有 CPU 频率锁定或排除其他桌面进程。

本机普通生产路径的 sharedtrans before qsort 未复现用户报告的 2046 ms：
这里 shm_open 为 EPERM，不能走真实共享 adoption。下述 file-backed 协议测试单列，
不冒充真实 POSIX-shm 生产路径数据。

最终 before 是从原始目录重新构建得到；重新构建的 SHA256 与测量二进制完全一致：
`4ab53074535c31d8bf8b8f40f1eee30d31c725b916bd752d3d7d269405a63b8d`。
after SHA256：`15b756ebd699f433575a691aa48b889d065c5a7d870297b4f9405c713b230b6c`。


## 修复后机制计数（独立诊断，非 timing 表）

`ATTESOR_FIX2_COUNTERS=1`，只额外打开所需 token，不开 STATS/PROFILE。
修复后的 k-arm64 日志在 `target/fix2/counters-after-{off,rel32chain,sharedtrans}-k-arm64.log`，
b-arm64 对应 `-b-arm64.log`。

| k-arm64 计数 | 原实现 rel32chain | 修复后 rel32chain |
|---|---:|---:|
| entry serialization / CPUID | 270840737 | 12661 |
| slot fallback | 270832605 | 2723 |
| slot link | 0 | 2723 |
| 发布前 rel32 link | 599 | 784 |
| graph busy | 原实现无此机制 | 0 |

计数仍有真实 rel32 边，不是偷偷把 token 改成 OFF。after epoch=0；本轮退化不能归因于反复失效。
sharedtrans 的 denied-shm 路径：prepare 3313→0、snapshot read 5086→0、mapping open 230→1；
adoption 均为 0，所以不能用这条路径证明真实 sharing 的性能或 adoption。

## 正确性与敏感性

最终正常源码分别运行：

```sh
cargo build --release --offline
python3 tests/attesor/fix2-check.py selftests
cargo test --release --offline --workspace -- --test-threads=1
ATTESOR_JIT_ON=rel32chain cargo test --release --offline --workspace -- --test-threads=1
ATTESOR_JIT_ON=sharedtrans cargo test --release --offline --workspace -- --test-threads=1
python3 tests/attesor/fix2-check.py sensitivity
```

- release build 通过；既有 warnings 仍在，不声称零警告。
- 两个 token 单独 ON × 14 类 × 100000 请求输入，全 **mismatch=0**。
  类别：blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks addrforms。
  编码随机类会跳过不适用输入：例如 blocks ok=8893/skipped=91107，不冒充执行了 100000 个有效块；
  loops/calls/fpnan/tbiblocks/addrforms 各 ok=100000、skipped=0。
- OFF、rel32chain、sharedtrans 三种配置，各 attesor **154 passed / 1 ignored**、core **9 passed**。
  唯一 ignored 是新增、显式要求单独运行的 guard/slot 性能诊断；它另外 `--ignored` 实际运行通过。
  difftest/doctest 无测试。日志 `target/fix2/tests-final-*.log`、`target/fix2/selftests-final.log`。
- 定向验证晚到 call、callret native return、执行 lease 覆盖 helper、graph busy 非阻塞回退、
  target 失效撤链、真实 fork child 锁重置、guard/body 入口分离、hot body/slot/constant bytes、
  Plain 本地链及失效、源快照边界、共享 producer/consumer 异址 adoption 与损坏/identity/容量门禁。
- 额外生产 dispatcher 测试先发布 source、后发布 target，运行真正 `run()` 的 EXIT_CONTINUE，
  检查 slot 被安装，并在下一次 native entry 直接执行到 target 的 SVC；不只直接调用 chain_slot。

敏感性脚本逐个改生产源码，均要求 **恰好 1 个测试的断言失败**（不接受编译失败或零测试），
`finally` 恢复后相同测试必须通过。8 个受控故障均被检出，记录 `target/fix2/sensitivity*.log`：

| 故障 | 检出点 |
|---|---|
| dispatcher 仍禁止 rel32 late link | 真正 run() 之后 slot 仍是 fallback |
| chain_slot 仍禁止 rel32 late link | 晚到 call 无法链接 |
| 缺完整 execution read lease | generated helper 可错误取得 exclusive lease |
| 普通入口仍指向 guard | canonical/body 分离断言 |
| Plain 跳转仍回 dispatcher | 没有有效 local exit_data slot |
| 每次重新 attachment | open 次数超出一次 |
| 每字单独源快照 | 单页 Mach read 不再是一次 |
| guard 改变 hot body 布局 | body/slot/constant pool 字节比较失败 |

没有遗留任何变异：运行时 mod.rs、sharedtrans.rs 与正常快照逐字节一致。
`guard_edge_cost` 是隔离 loop 的辅助诊断，不是所有 rel32 实现固有慢的证明；不能据此丢弃 #20。


## 共享池实际可连接路径（test-only file backing，交错 3 轮）

生产 `shm_open` 被 sandbox 拒绝，因此在 **cfg(test)** binary 用普通文件承载相同 MAP_SHARED
协议；不是在 release 生产 binary 偷换 shm，也不是把 test backing 宣称为 POSIX-shm 验收。
before 是原实现加 test worker 和机制计数钩子的版本，未改翻译或 chaining 行为；完整 shim 在
`target/fix2/baseline-test-shim.patch`。每次测量使用 fresh pool，比较的是 producer/local Plain
路径；跨独立进程 consumer adoption 另由 `g4_sharedtrans_two_independent_processes_adopt_without_retranslation` 验证。

```sh
python3 tools/fix2-measure.py \
  --before target/fix2/final/attesor-test-before --after target/fix2/final/attesor-test-after \
  --guests target/fix2 --out target/fix2/final/file-protocol.json --runs 3 \
  --test-pool target/fix2/final/pools
```

全部 24 次完整 guest 执行 rc=0。原始日志/命令/每轮毫秒见 `target/fix2/final/file-protocol.{json,log}`。
中位数 ms，所有样本均纳入：

| guest / kernel | before OFF | before shared | after OFF | after shared | after shared/OFF |
|---|---:|---:|---:|---:|---:|
| k-arm64 / int-hash | 447.0 | 448.9 | 447.9 | 437.6 | 0.977 |
| k-arm64 / qsort | 1232.2 | 2243.9 | 1250.4 | 1269.0 | 1.015 |
| k-arm64 / fp-nbody | 131.0 | 134.4 | 129.6 | 120.3 | 0.928 |
| k-arm64 / memcpy | 33.3 | 34.4 | 34.3 | 33.0 | 0.962 |
| k-arm64 / branchy | 1235.2 | 1222.2 | 1223.8 | 1214.9 | 0.993 |
| b-arm64 / fib(36) | 31.0 | 76.0 | 32.0 | 32.0 | 1.000 |
| b-arm64 / qsort | 436.0 | 700.0 | 419.0 | 433.0 | 1.033 |
| b-arm64 / sieve | 518.0 | 483.0 | 470.0 | 501.0 | 1.066 |
| b-arm64 / crc32 | 293.0 | 288.0 | 285.0 | 285.0 | 1.000 |
| b-arm64 / list | 276.0 | 257.0 | 268.0 | 275.0 | 1.026 |
| b-arm64 / matmul | 101.0 | 105.0 | 100.0 | 103.0 | 1.030 |
| b-arm64 / memops | 298.0 | 295.0 | 298.0 | 295.0 | 0.990 |
| b-arm64 / TOTAL | 1951.0 | 2221.0 | 1873.0 | 1921.0 | 1.026 |

独立机制诊断 `target/fix2/file-counters-after.log`：229 个本地 Plain、1 次 mapping open、
3529 次源快照 read、13202 次 dispatcher entry。对照原始诊断的 215 个 Plain、30006746 次 dispatcher entry，
证明本地链槽消除了热 Plain 边永久回 dispatcher 的机制；guest/ASLR 导致 unit 数不同，不把差异当作一模一样的执行轨迹。

主要 qsort 退化可在这条协议路径复现并修复：k 2243.9→1269.0 ms，b 700→433 ms。
但 b/sieve 的 501/470=1.066 等残余也如实列出，**这一阶段同样未达到逐 kernel 全不慢的严格目标**。
不能用 TOTAL 或 qsort 的改善掩盖其它行。

## 残余差距调查及未采纳实验

1. 无 token 与 denied-shm 的 sharedtrans 使用相同普通 JIT 生成路径；后者仅首次 attachment 后缓存失败。
   小百分比差距还不能区分调度、频率、ASLR/缓存布局和翻译开销；不能仅凭“代码相同”把所有实测差距删掉。
2. immutable rel32 仍需存活/epoch guard；canonical slot 不需要这个附加 guard。
   这是额外机器指令的事实，但**不足以证明运行时间必然更慢**。新增隔离性能诊断
   `fix2_rel32_guard_edge_cost`（7 轮、每次 10000000 条边，交错顺序）实际执行并校验计数；
   该 memory-RMW loop 中 guarded-direct 反而略快，说明不能拿“多几条指令”冒充不可优化的证明。
3. 为检验 global code layout 假设，另建隔离实验：guard 从 code-cache 侧池分配，不再改变后续 body 分配位置。
   release 构建通过；3 轮 current/sideguards × OFF/rel32 × k/b 交错测量记录在
   `target/fix2/sideguards.json`，实验 diff 在 `target/fix2/rejected-sideguards.patch`。
   未采纳：sideguards b/fib 34 vs OFF 29 ms，b/qsort 432 vs 420，b/sieve 480 vs 452；
   相应 current 为 33/32、431/418、462/443。它没有满足全 kernel 要求，并引入侧池容量/局部性权衡。
   未把这个未通过性能门禁的实验合入生产实现，也不宣称它经过完整正确性验收。

**结论：主要 dispatcher/serialization 与共享 Plain 永久回 dispatcher 的机制已消除，正确性检查通过；
严格的每 kernel ON≤OFF 目标仍未全部达成。没有固有慢的充分证明，不丢弃或默认禁用两项功能。**

## 交付与重放

- `docs/FIX2-REPORT.md`：本报告；原始证据均保留在 `target/fix2/`。
- `target/fix2.patch`：对原始 `../attesor-fix2-orig` 的统一补丁，包含新增文件内容；不含 .git、target 或实验实现。
- 用 `diff -ruN original fixed` 生成（`-N` 确保新增源码/脚本/文档不是仅一条 “Only in”），无需 git。
- 在另一个 `cp -R` 的原始副本上实际 `patch -p1` 重放，并逐文件比较 fixed；不会写入原始副本。

```sh
# 在原始副本的另一个可写 copy 中；无需 git
patch -p1 < /Users/USER/attesor-fix2/target/fix2.patch
cargo build --release --offline
python3 tests/attesor/fix2-check.py selftests
python3 tests/attesor/fix2-check.py sensitivity
python3 tools/fix2-summary.py target/fix2/final/production.json
```

最后一条 summary 命令需要本轮的 target 证据目录（补丁不携带二进制或测量 JSON）。
