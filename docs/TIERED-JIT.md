# 冷块快译与热块升级（实验，默认关闭）

## 配置与范围

- `ATTESOR_JIT_ON=tiered`：精确匹配逗号分隔 token，启用本策略。
- `ATTESOR_JIT_OFF=tiered`：同样精确匹配，优先于 ON；不是子串匹配，不自动去空格。
- `ATTESOR_JIT_TIER_THRESHOLD`：正 `u32`，默认 32；0、负数、溢出或非法文本回退默认值。
- 配置首次访问后在进程内固定。`ATTESOR_JIT_THRESHOLD` 仍仅控制未启用本功能时的旧解释器预热。
- JIT 关闭时保持解释执行；`ATTESOR_JIT_VERIFY` 启用时使用原有验证策略，不使用 tiered。
- AOT 已命中的代码保持完整优化策略，不降级、不计入冷译计数；AOT builder 不使用快译。

## 生产路径

首次 JIT miss 立即生成 x86 机器码，不以解释器预热代替快译。快译仍使用现有逐指令 native translator 和必要的已有 fallback helper，但不建立 lookahead 窗口、不计算 next-use、不做 flags/AES 双指令融合、跨块 flags-dead/vector-dead 扫描、loopra/regioncache 发现、调用 continuation inline、host call/return 或块内回边。每块仍最多 96 条 guest 指令；FPCR 安全扫描仍保留。

冷译直接和间接出口都返回规范 dispatcher（`exit_data=0`），不生成可直链出口；冷译目标也不发布到 IBTC 或 incoming chain。这样自循环、热块到冷块、BR/RET 都不能绕过冷译返回入口统计。返回一次计数一次；FPCR `EXIT_INTERP` gate 不计，普通指令级 fallback、SVC 或 pending exit 仍算一次返回。计数不是完整指令数或吞吐量。

计数达到阈值后，在下一次该 PC 的规范 dispatch 同步重译完整单元；没有后台队列，也没有未消费的升级请求。完整编译从新的 `guest_reader()` 重读 guest 指令，不重用冷译机器码或保存的指令快照。达到阈值但不再进入的块不会重译。

## 代际、SMC 和并发

- 冷译状态位于 `JitIndex.cold`，与 thread-local 解释器 HOT 完全分离。唯一 host allocation 作为代际身份；计数饱和增加。
- 升级请求携带 `(old_host, epoch)`。获取 `TRANSLATE` 后重新核验当前 BLOCKS、epoch、冷译身份和阈值；不符不覆盖新代码，无当前代码时重新冷译。无关失效也会保守取消本次升级，下一次入口可重试。
- 编译、升级、SMC 和链补丁都使用现有 TRANSLATE 序列化；只在规范 dispatcher 返回后的 Rust 栈升级。发布前再次比对 BLOCKS，拒绝覆盖并发 AOT 注册。
- SMC 删除匹配 host 的冷译计数；旧线程的返回只可增加匹配 host 的记录，不能污染新代。
- 升级解绑所有已追踪旧 host incoming slot，发布新入口并递增 EPOCH。当前线程在执行前重新观察 epoch、清 IBTC；其它线程沿用现有 dispatcher/ISB/host continuation 的 epoch 协议。
- 旧代码、旧 UnitRec、slot 和常量不回收、不复用；旧冷译最多执行当前有界块后返回。现有执行中的优化代码并非被异步强停，沿用项目原有指令缓存同步前提。
- 本次集成生成码版本 62；AOT identity 增加 `aot-full-only;tiered-quick-v1`，既有 ON/OFF 环境身份继续隔离模式。冷译不写盘；内存冷/热策略通过独立冷译表和不同 host 代际区分。阈值不改变生成码，因此不进入磁盘 identity。

## 已知限制与验收状态

这是保守同步子集：没有后台优化、分级热度衰减、旧代码回收或内存预算。升级持有全局编译锁，快译强制 dispatch 与双份机器码可能反而增加运行成本；未测得收益，不能默认开启。

原始补丁仅通过离线编译。本次移植已增加并执行 L0、定向执行及生产升级禁用的敏感性测试，详情见 `PORT-G2-REPORT.md`；未做性能、GUI 或 L1/L2 验收。依赖项目既有 guest 可读性、SMC invalidation/IC+ISB 同步以及 fatal fault 前提；不能保证任意未同步并发 guest 写入的快照一致性。AOT 并发 cache allocation 的现有 cursor 断言/非索引 AOT SMC 范围不在本功能内修复。

可观测性：`ATTESOR_STATS` 的新增 `tiered: quick_blocks=… promotions=… threshold=…` 是运行时诊断计数，不是验收结论。

## 与 wave-1 / 其它 G2 开关组合

精确依赖收集、jitcost、slotreuse 与 fastenc 包装保持原有契约。启用 bgtrans 时，已完成的后台完整单元像 AOT 命中一样保持 full，不加入冷计数；冷单元没有可排队出口，升级后的 full 单元可排队直接后继。磁盘 AOT builder 始终从默认 `Unit::new()` 建立 full 单元。
