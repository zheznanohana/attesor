# isel2：可选主机 ISA 指令选择

本功能为独立实验开关，默认关闭：`ATTESOR_JIT_ON=isel2`。
`ATTESOR_JIT_OFF=isel2` 优先于 ON；名称按精确逗号分隔 token 匹配。
选中特性在进程内只读快照，供 JIT/AOT 发码和 AOT identity 共用。

## 生产实现范围

- BMI2：32/64 位 LSLV、LSRV、ASRV 选择 SHLX、SHRX、SARX。
  三操作数读取两个源后写结果，region 已分配源不强制复制到 CL；
  count 使用指令宽度的低 5/6 位，32 位结果零扩展。RORV 保留旧模板。
- LZCNT：32/64 位 CLZ 用 LZCNT；包括零输入返回 32/64。
- LZCNT：32/64 位 CLS 先规范化符号（`x XOR (x ASR (width-1))`），
  再 LZCNT 减 1；零和全 1 输入返回 width-1。
- 每种扩展独立运行时检测。缺 BMI2 时变量移位保留 CL 模板；
  缺 LZCNT 时 CLZ 保留 BSR 模板，CLS 保留解释器 fallback。
- 普通 IEEE CRC32 不等于 Intel CRC32（Castagnoli）；本功能不新增 CRC 模板。
  SIMD CNT 仍解释器 fallback；bitfield 现有实现不替换；不引入 POPCNT/SSE4.2 依赖。

AOT 头 flags 为实际选中的 BMI2、LZCNT 各保留一位，翻译器 version 升级；
不仅依靠 `ATTESOR_JIT_ON` 字符串区分主机。NZCV 存储仍由原机制管理，
本功能不声明新的跨指令 x86 flags 保活能力。

## 验证边界

本轮只允许生产实现与必要离线编译；未新增或运行测试、自测、差分、变异、
bench、guest、GUI、硬件验收。编译成功不代表行为正确或提速。
Claude 后续验收应覆盖：别名与 ZR、32 位高半清零、count 0/31/32/63/64/全 1、
CLZ/CLS 零及符号边界、region/non-region、NZCV 不变、ON/OFF 优先级、
缺扩展回退及 AOT 跨主机 flags 不匹配拒载。

## integ1 移植验收

上述“仅编译”描述是原功能分支交付时的历史状态。2026-10-30 的 integ1 移植已补充
定向执行、解释器对照和恢复式语义变异测试；有效 ISA 位改用 12/13，统一 translator version 为 64。
范围、实测结果及仍未验证的部分见 [PORT-G4-REPORT.md](PORT-G4-REPORT.md)。没有性能收益声明。
