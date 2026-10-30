# G2 移植报告（2026-10-30）

## 结果与纪律

按 **#11 → #12 → #13** 完成，**无跳过项**。三项前提在 integ1 仍成立，但旧补丁不能原样覆盖
wave-1 的 BlockRegistry、精确依赖、AVX-512 flags、offline AOT、jitcost 和 slotreuse。
采用手工移植（没有使用会写 index 的 `git apply --3way`），保留接手时已有的 staged 改动。
没有 git stage/commit/reset/merge/stash 等写操作，没有默认开启新功能，没有访问用户应用缓存。

三个精确逗号 token 分别是 `fastenc`、`tiered`、`bgtrans`；全部默认关闭，OFF 优先，不 trim、
不接受子串。既有 codegen_id 混入完整 ON/OFF/DISABLE 环境字符串，三项均被隔离；另保留
full-only tiered policy identity。`TRANSLATOR_VERSION` 在三项完成后**只改一次：61 → 62**。

交付入口：`target/port-g2-commit-message.txt`。完整原始日志：`target/port-g2/logs/`。
可重复定向检查：`python3 tests/attesor/port-g2-check.py all`。

## #11 fastenc：完成

- 新增有界 allowlist byte encoder；每个命中仍对应一个 iced DeclareByte，由 iced 处理布局、
  标签、分支及 miss。保留完整 assembler，混合失败回退原 iced。
- `Unit::assemble_options` 接入 JIT 初次/对齐重汇编、AOT offline 管线原始单元汇编及 standalone
  call fixture。没有绕过 wave-1 的 offline DSE/constprop；当它们产生优化列表时，最终列表仍由
  原有 BlockEncoder 编码，不声称 fastenc 加速了那一阶段。
- 原补丁 bit 6 已被 AVX-512 占用，改用 **bit 9**，不改变现有 bit 6–8；启用时文件后缀
  `-fastenc`，关闭时保留默认命名。imm64 地址、带标签模板、复杂 memory、前缀与 relocation
  metadata 选项仍回退。slotreuse 要求 relocation infos 时也保守回退。
- 定向测试覆盖所有 allowlist Code、32/64 位寄存器、REX、RSP/R12 SIB、RBP/R13 零位移、
  disp8/disp32 边界、signed immediate：**317,952** 个 memory/immediate 组合及额外寄存器组合，
  均独立对照 iced Encoder 字节。另测混合标签、多个基址、metadata 回退、实际执行与缓存位/命名。
- 敏感性：故意把生产 REX.W 的移位 3 改为 2，测试 exit 101，检出 `44 01… != 48 01…`；
  恢复后通过。另执行损坏 imm32 的代码，观测 13 而非期望 12。

逐项验证：release build；workspace L0（当时 attesor 113 + core 9）；dpimm/dpreg/ldst/branch/blocks
各 20,000 例，全部 mismatch=0；默认、ON、ON+OFF、伪 token 检查通过。
日志：`11-build-final.log`、`11-tests-final.log`、`11-*.log`、`11-mutation.log`。

## #12 tiered：完成

- 冷块直接生成 quick 单元，关闭 lookahead/fusion、跨块死值扫描、region、inline/callret、
  内部跳转；直接和间接出口返回 dispatcher。保留 FPCR gate、必要 helper 和精确 reader 依赖。
- INDEX 中按 PC + host generation 计完成返回数，达到正 u32 阈值后在下一规范入口同步重读并
  生成 full 单元。保留 wave-1 计时、依赖记录、对齐/slotreuse 和 BlockRegistry 发布方式。
- 冷目标不进 IBTC/incoming chain；升级检查 host/epoch，防止覆盖 AOT 竞态，解绑旧 incoming，
  推进 EPOCH，保留旧代码。SMC 只删除匹配 host 的冷记录，晚返回不能污染新代。
- AOT builder 的 `Unit::new` 始终 full，不持久化 quick。阈值不改变生成码，不进入 cache identity。
- 定向测试实际执行 cold/full，验证阈值前不升级、跨线程完成计数、过期 epoch 取消、重新读取
  指令、SMC/旧 host 晚返回、链解绑，以及预填真实 IBTC 命中时 quick BR/RET 仍返回 dispatcher。
- 敏感性：故意将生产 promote 条件禁用，测试 exit 101，检出 host 未升级；恢复后通过。
  私有 guest 字节故意不失效，旧代码执行 11，重读升级后执行 12，证明不是克隆旧机器码。

逐项验证：release build；workspace L0（114 + 9）；blocks/loops/branch 各 20,000 例，mismatch=0；
ON/OFF/exact、0/溢出阈值、verify、fastenc/preciseinvalidate/slotreuse/jitcost 组合通过。
日志：`12-build-final.log`、`12-tests-restored.log`、`12-modes.log`、`12-mutation.log`。

## #13 bgtrans：完成

- 保留原补丁的真实低 QoS worker、owned 4 KiB 页、单块 staging、队列/尝试/字节预算和非阻塞采纳。
- 接入当前发布/查找管线；在所有 invalidation 早退前推进 speculative generation。后台 UnitRec
  带 host_bytes 与完整页 source_ranges，参与当前精确失效/页索引/链解绑。
- 在 BLOCKS 锁中复核 AOT 发布，不覆盖新入口；profile 记录在取得该锁之前，避免反向取得 IMAGES。
- 预热 wave-1 新增的 AVX-512、loopra2、jitcost、fastenc/census 配置；loopcensus 下停用 worker，
  避免它在后台进入符号归因/诊断 I/O。relocation 选项仍使 fastenc 回退 iced。
- fork gate 在原 fork 锁之前；guest fork 等当前有界 emission，可选 AOT fork 忙则跳过；子进程
  停用继承 worker，不碰旧 queue/Condvar/OnceLock。退出不 join，失败 exec 保留原进程能力。
- 定向测试使用真实 worker：前台 finished_stubs → 后继排队 → worker 产物 → lookup 采纳执行。
  测多个最终 host 基址与 slot continuation、VM 完整/部分失败、无 victim 失效、整页依赖、
  内容/commpage 拒绝、TRANSLATE 忙时不等待、真实 guest fork 与子进程继续前台翻译、父进程继续采纳。
- 敏感性：故意去掉生产 source 内容复核，测试 exit 101，检出 changed source 被采纳；恢复通过。
  旧 host 确实执行 7，新代码执行 42，不只检查 metadata。

逐项验证：release build；workspace L0（117 + 9）；blocks/loops/calls/fpblocks 各 20,000 例，mismatch=0；
默认/OFF/exact/verify/诊断/census/全组合定向矩阵通过。
日志：`13-build-final.log`、`13-tests-restored.log`、`13-modes.log`、`13-mutation.log`。
普通随机 self-test 本身不启动 worker；真实 worker、排队及采纳覆盖来自上述定向执行，不能混为一谈。

## 最终集成回归（版本 62）

- `cargo build --release -p attesor`：成功（`final-build.log`）。
- `cargo test --release --workspace -- --test-threads=1`：默认与
  `fastenc,tiered,bgtrans,preciseinvalidate,slotreuse,jitcost`（threshold=3）均 **117 + 9 通过，0 失败**。
  日志：`final-default-tests.log`、`final-combined-tests.log`。
- `port-g2-check.py all`：20 个选项/执行检查 + 6 个 fresh-process identity 检查通过。
  同一测试二进制的 default/fastenc/tiered/bgtrans/all/ON+OFF codegen_id 六者均不同；flags bit 9 与
  默认命名也分别核验（`final-directed.log`、`identity-*.log`）。
- 三 token 同开：dpimm/dpreg/ldst/branch/blocks/fpblocks/fuse/fuse2/selchain/tbiblocks/fpnan/loops/calls，
  每类 20,000 个输入，均 mismatch=0；各类 unsupported/skipped 如日志原样保留，不当作 native 通过。
- `fastenc,slotreuse,callret`：jitcode 20,000 例，mismatch=0，八种 host 对齐余数全部覆盖。
- `fastenc,offline-dse,offline-constprop`：offline 20,000 例，mismatch=0，实际磁盘加载 130,222 个入口，
  消除 8,890 次 store。jitcode 用 full/slot 模式测试，不把无 slot 的 tiered quick 当作 slot 重定位验收。
- `git diff --check` 通过；Python 定向 runner 编译检查通过（pycache 指向本树 target）。

## 测试中发现并修复的问题

- 新定向测试初版使用了 iced code_asm 不导出的 `rip` 变量，改为受支持的无 base memory 构造。
- 原独立 callret fixture 没发布到 BLOCKS；tiered 新增的生产 IBTC ownership 检查正确拒绝它。
  fixture 改为直接初始化自己的 IBTC，生产检查不放宽。
- 原精确失效测试假设 incoming 总能直链；启用 tiered 时改为断言冷目标保持 continuation。
- 最终全组合测试发现，较早的后台 SMC fixture 已按设计永久关闭 external-AOT latch，影响后续
  原测试的初始假设；该测试现以 RAII 保存/初始化/恢复测试进程 latch，不改变生产 one-way 合约。
- 所有上述失败均已修复并重跑相关 gate。三个 `*-mutation.log` 中的失败是有意的负控制，生产源码已恢复。

## 剩余边界

这是 opt-in 的 L0 交付，不是默认启用批准。未做 L1/L2、真实 GUI/应用、性能 benchmark 或收益评估。
构建仍有 unused/deprecated、函数 cast、Mach FFI 签名及 SDK/temp 环境警告；未扩大范围清理。
VM copy / 字节复核不是任意并发写入/重映射的原子快照，仍依赖现有 IC/ISB 和失效合约。
真实 fork 可以受低 QoS emission 调度影响；有界工作量不保证墙钟延迟。
保留旧代码、双 tier 与后台投机均可能增加内存/延迟；没有性能改善结论。
