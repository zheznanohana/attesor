# 转译器研究：可迁移到 Attesor 的下一批优化

研究日期：2026-10-29。Attesor 审查基线为 `c57a0683917597ac62bc011bf6e2e7086f4ed5a5`。
本报告只研究和提出可验证方案，没有修改生产代码、运行性能基准或声称新的速度收益。
DSH 与 ARM VS Code 是代表性回归样本；实现不得按应用名、bundle ID 或具体版本打补丁。

## Rosetta 能确认的设计与证据边界

Apple 官方说明 Rosetta 同时使用进程内 JIT 和持久 AOT；运行时查询服务获取匹配的翻译产物，
产物与原始镜像身份绑定，服务以只读文件描述符分发缓存。可以借鉴自动预热、严格身份匹配和
透明启动的整体设计。文档没有公开寄存器分配器、内部 IR 或可移植的性能比例；不能据此宣称
某项开源实现就是 Rosetta 的内部算法。[Apple Platform Security：Rosetta 2](https://support.apple.com/en-sg/guide/security/secebb113be1/web)

Attesor 已有按镜像/生成器/宿主特性隔离的缓存、profile 合并、检查点和本轮静态预热。
重复实现“有 AOT”不会带来新收益。下一步应测 DSH/VS Code 的实际翻译耗时、缓存命中和完整
页面就绪，而非只统计 `.aot` 文件或静态发现的块数。Apple 的内核/签名特权不能作为普通
用户态代码可直接照搬的接口。

## 优先级与具体落点

下表的收益是待检验的工程判断，不是引用他人基准得到的 Attesor 提速预测。

| 优先级 | 借鉴点 | 当前 Attesor 缺口 | 第一份可验收交付 |
|---|---|---|---|
| P1 | 消除比较到跳转之间不必要的条件物化 | CBZ/CBNZ、TBZ/TBNZ 经 `setcc al` 再 `test al` | 直接 Jcc，保留 guest NZCV；覆盖两条分支、32/64 位、bit 0/63、XZR 和各种缓存状态 |
| P2 | 精确 next-use 与跨边缓存状态 | 五个 GPR、八个向量读缓存采用 LRU，10 条 lookahead 按原始字段粗猜；每块清空 | 先做精确读写集和 next-use 淘汰，保持写通；再做单前驱 region 和受控边转换 |
| P3 | CFG 上的 flags 活跃性 | 仅相邻融合和有限前瞻；许多 setter 仍立即写四个 NZCV 字节 | 对一个小 region 计算每个旗标的 live-in/live-out，先扩死写消除，再延迟物化 |
| P4 | 分层间接目标缓存与完整失效协议 | 每线程 direct-mapped IBTC；miss 才进入全局查找；epoch 仅回 dispatcher 时检查 | 先记真实 miss/collision，再评估小 L1 + 共享/页式 L2；同时修纯 IBTC 循环的失效观察 |
| P5 | 由 ABI 描述驱动的原生库桥 | 已有 GPU 桥和四个无状态 zlib API；尚无通用 callback/所有权描述 | 先按 profile 扩纯缓冲 API；增加类型、布局、所有权和错误语义的自动生成/验证 |

### P1：直接条件跳转，先拿确定的冗余指令开刀

`crates/attesor/src/jit/emit.rs::branch_sys` 的 CBZ/TBZ 类先设置 `al`，随后
`cond_branch` 再测试 `al`。可直接把 `cmp/test/bt` 的结果交给 x86 Jcc，再使用已有 `goto`。
这项具体冗余是对当前源码的独立观察。QEMU 把分支作为 IR 终结操作，并利用活跃性删除死的
中间结果；这支持避免不必要的布尔中间值，但不是对本方案收益的证明。
[QEMU TCG IR：Basic Blocks、Code Optimizations、Jumps](https://github.com/qemu/qemu/blob/73294d096cc591e122fde9e63db4739de6d84f2b/docs/devel/tcg-ops.rst)

验收应增加定向多块差分和指令形态检查：有效原生路径不应残留那组 `setcc/test`；同时比较
`bench/native-ratio/br.c`、间接调用和综合内核。不要只运行会退回解释器的随机编码。

### P2：先改淘汰质量，再把缓存寿命扩过控制流边

FEX 的当前寄存器分配 pass 使用 next-use、furthest-first spill 和常量重物化，支持 preferred
register 与 tied operand。Box64 的 `CacheTransform` 则在分支目标处协调浮点缓存和 flags
状态，并按目标需要选择搬移或清空。两者提供了不同层次的借鉴。
[FEX RegisterAllocationPass.cpp](https://github.com/FEX-Emu/FEX/blob/fb48e2d19de4bd49bbc66d42242609357883a302/FEXCore/Source/Interface/IR/Passes/RegisterAllocationPass.cpp)
[Box64 CacheTransform](https://github.com/ptitSeb/box64/blob/30d5c77188fa9cf8bea7064d93ced554c3a34cde/src/dynarec/arm64/dynarec_arm64_helper.c#L2123-L2454)

Attesor 落点是 `rc_alloc`、`vc_alloc`、`used_soon` 和 `emit_block`。先用实际操作数读写集取代
“任意字段碰巧等于寄存器号”的猜测，再用更远 next-use 取代 LRU；这一阶段不改变 CPU 内存
写通规则。后续只在同一个翻译 region 内，对确定的单前驱边保留缓存，多前驱 join 显式协调，
helper 和外部入口回到规范状态。不能简单删除块入口的 `RegCache::new()`。

更激进的写回缓存需要 host-PC→guest-PC 和寄存器位置映射，才能在 fault、调试和信号观察时
重建准确状态。QEMU 明确维护这种异常映射；LLVM stack map 记录特定位置的 live value 地址，
是可参考的元数据思路，并不要求把整个后端替换为 LLVM。
[QEMU：Exception support](https://www.qemu.org/docs/master/devel/tcg.html#exception-support)
[LLVM 18.1.8 stack maps](https://releases.llvm.org/18.1.8/docs/StackMaps.html)

门禁必须包含 diamond/loop 的每个入口、寄存器别名、helper clobber、故意访存故障和
自修改代码失效。仅吞吐变快而恢复状态不正确不能接受。

### P3：先做完整活跃性，再谈 lazy flags

FEX 的 `RedundantFlagCalculationElimination` 为操作描述 flags 的读写集，建立前驱 CFG，
用 worklist 传播活跃性，然后消除不需要的 flag 计算和折叠比较分支。
[FEX 当前实现](https://github.com/FEX-Emu/FEX/blob/fb48e2d19de4bd49bbc66d42242609357883a302/FEXCore/Source/Interface/IR/Passes/RedundantFlagCalculationElimination.cpp)

Attesor 已有 `flags_dead_at`、`flags_dead_after_selects`、compare/branch/select 融合。
优先把这些有限前瞻替换为小 region 的保守读写分析，扩大确定死掉的 N/Z/C/V 写入消除；
再考虑保存 producer/操作数并在真正读取时物化。必须处理 `MRS NZCV`、ADC/SBC、CCMP、
多个消费者、helper 和 region 退出。Box64 的 deferred/native flags 开关可借鉴测试矩阵，
但 `SAFEFLAGS=0` 的调用/返回假设不能成为正确性默认值。
[Box64 flags/CALLRET 参数说明](https://github.com/ptitSeb/box64/blob/30d5c77188fa9cf8bea7064d93ced554c3a34cde/docs/USAGE.md#box64_dynarec_df)

### P4：改 IBTC 之前先确认命中损失和失效正确性

FEX 当前 lookup cache 将 L1 与按 guest 地址页索引的 L2 分开。Attesor 的 32,768 项 IBTC 已经
是每线程缓存，直接跳转也已有 patchable slot 和 incoming unlink；“增加基本块连接”本身
不是当前的新优化。[FEX LookupCache.cpp](https://github.com/FEX-Emu/FEX/blob/fb48e2d19de4bd49bbc66d42242609357883a302/FEXCore/Source/Interface/Core/LookupCache.cpp)

落点为 `jit/mod.rs::{ibtc_insert,ibtc_clear,run,invalidate_range}` 与 `emit.rs::indirect`。
先对多个相同索引的函数目标、随机间接目标和真实 renderer 记录 miss/collision。若确有问题，
测试 2-way L1 或便宜 L2，而不是单纯继续增大表。当前 epoch 检查位于 dispatcher，纯命中
循环可能延后看到失效；优化 lookup 不能把这个已知风险扩散。QEMU 的实现说明同时跟踪
翻译页和直接链反向关系，强调失效与链接属于同一个协议。
[QEMU：失效及直接链](https://github.com/qemu/qemu/blob/73294d096cc591e122fde9e63db4739de6d84f2b/docs/devel/tcg.rst)

### P5：扩原生库时借鉴类型和所有权，不只增加函数表

FEX 支持 guest→host thunk 和 host→guest callback，并从接口声明生成打包代码。其项目曾
因 guest/host 分配器混用而修正指针所有权；签名、结构布局和回调路径都是必要基础设施。
[FEX ThunkLibs README](https://github.com/FEX-Emu/FEX/blob/fb48e2d19de4bd49bbc66d42242609357883a302/ThunkLibs/README.md)
[FEX 官方：指针所有权与 callback 问题](https://fex-emu.com/FEX-2208/)

Attesor 的 `native.rs` 与 `native/zlib-shim.c` 目前避免跨界传 `z_stream`/callback，边界合理。
下一个库应由 DSH/VS Code 的热栈决定，优先无需 guest allocator 和回调的缓冲计算 API。
加入流式压缩、密码上下文或数学库前，要写明分配/释放方、结构布局、errno/错误码和 FPCR
行为。GPU Metal 桥应维持原回归基线；不能为了减少 CPU 指令而绕过已有桥。

## 不直接照搬的部分

- ARM→x86 的 guest GPR 比可用 host GPR 多。不能照搬反方向的静态一对一寄存器映射，
  也不能仅因本机有 AVX-512 就把全体 SIMD 指令/缓存改为无特性检查的 EVEX 路径。
- Box64/FEX 的 x86→ARM 内存序策略不能当作本项目的性能旋钮；ARM barrier 和 atomic 仍须
  按语义验证。当前 `try_atomics` 已利用 x86 的 acquire/release 条件，不能无条件删 DMB。
- 当前 `host_call` 的 CALLRET 以前测得更慢，且包含 epoch/返回目标校验和安全深度限制。
  在没有新设计和独立收益证据前保持可选；不要通过删正确性检查“优化”。
- 大 region 会增加首次翻译成本。FEX 官方 2022 年曾因 JIT 开销关闭 multiblock 默认；这是
  历史设计教训，不是对当前版本默认值的断言。冷启动和热运行必须分开验收。
  [FEX 2212：编译开销与 multiblock](https://fex-emu.com/FEX-2212/)
- Rosetta 的系统身份、签名和服务权限不是这个用户态项目能获得的等价接口。没有同工作负载、
  同条件的测量时，不给“达到官方百分之多少”的估计。

## 统一验证方法

先通过原生路径覆盖、独立参考和原有总门禁；每个候选提供 OFF/on 同二进制开关，单独串行
运行至少 7 组交替样本，保留全部数据和代码哈希。比对无效编码的 skipped 数量及 native 数量，
防止用解释回退“通过”。微基准通过后，再沿 Claude 的标准比较 DSH 的隔离冷/热启动和真实
窗口，以及 VS Code 工作台就绪、输入/滚动帧延迟与 GPU 状态。全程保留现有 Metal/ANGLE
参数，不用应用白名单或关闭正确性保护换成绩。

源码快照版本和 SHA-256 见
[研究来源清单](measurements/2026-10-29-translator-research-sources.json)。
