# 离线 Cpu GPR 常量转发（默认关闭的实验切片）

`ATTESOR_JIT_ON=offline-constprop` 在真实离线 `jit::offline::assemble`
路径启用独立后发射 pass；`ATTESOR_JIT_OFF=offline-constprop` 优先，默认关闭。
它可单独启用，也可与现有 `offline-dse` 组合；组合时先转发再执行旧 DSE。
普通 runtime JIT 路径不使用本 pass。

## 已实现

- 在 host 指令直线段内记录普通 MOV 立即数及其 GPR 副本，以及完整 64-bit
  `Cpu.x[0..32]` store 的已知值；值不是根据数值范围或地址外观猜测。
- 只将精确的 64-bit MOV load 和同槽低 32-bit MOV load 替换为 MOV immediate。
  32-bit host MOV 保留零扩展，内存 imm32→64 store 保留符号扩展；不改 NZCV/host flags。
- 每个 symbolic label、任意控制流转移、helper、未知指令、非 Cpu GPR 内存观察点、
  Cpu base 写入均清空状态。部分/条件 Cpu GPR 写使所有重叠槽失效。
- pass 接受重定位指令排除列表。当前树只有固定地址 MCLAOT05，没有
  `Unit.address_insns` 或加载时 image relocation；生产传入空列表。未来增加
  relocatable AOT 时，必须先接通该元数据，不能直接复用本调用。
- 保持指令个数/符号 IP 索引；现有 BlockEncoder 负责实际偏移与分支松弛。
  encoder 失败时整体回退未优化 chunk，不发布部分结果。
- 整批移植统一递增 translator version 60→61；既有 codegen identity 包含 ON/OFF 字符串与 binary UUID，
  实验缓存不会与默认缓存混用。
- `ATTESOR_STATS` 可输出每 chunk `forwarded_cpu_loads`，只计成功编码的转发。

## 明确未实现／未验证

这不是完整函数 IR，也不是跨块/跨 join 常量传播。不推导 ADR/ADRP 地址常量、
算术结果、向量或 flags；任何 label 都清空，因此不给循环和外部入口虚假的事实。
不会替换 guest memory load，不承诺缩短机器码或提升性能：64-bit imm64 可能比原
load 更长，是否值得启用须由 Claude 后续验收决定。

integ1 已执行 release build、10 项 offline L0（含定向执行及错误常量变异检测），
以及私有 native/interpreter/JIT/磁盘 AOT 集成检查。日志见 `target/port-g1`。
这些检查不等于真实应用、所有 ASLR 模型、L1/L2 或性能验收；未测量收益。

## integ1 port

This tree uses fixed-address MCLAOT05 images and has no Unit.address_insns or
load-time image relocations. The forwarding pass therefore receives an empty
relocation exclusion list. Relocatable AOT must supply that metadata before reuse.
The jitcost entry marker is a forwarding barrier (not a Cpu GPR slot).
