# 第二轮优化与验收（2026-10-29）

**本轮代码已合并、安装，21 组自动化验收全部通过。完整图形交互验收尚未完成，不能宣称达到官方 Rosetta 的兼容性或性能。**

安装的翻译器 SHA-256：`c19e12ad398809cec1e9cc2bb3a4610c1dff7d34f2686a3629126665a1814550`。
AOT translator version 为 41；旧构建/选项的机器码缓存不能继续使用。

## 八个方向的交付状态

| 方向 | 当前已落地、验收的范围 | 明确边界 |
|---|---|---|
| JIT 代码质量 | TBI 掩码复用；CBZ/TBZ 直接条件跳转；按寄存器 bank 的 next-use 分配；修复 NZCV 活跃分析跨越故障/helper；可选自然循环 GPR 缓存 | regioncache 默认关闭，只覆盖单块自然循环；没有一般应用提速证明 |
| 提前翻译与缓存 | `.app` 主程序/Helper 与 dyld 静态 prewarm，不执行客体；实际载入验证；版本/开关隔离；强退、fork、失败 exec、重建重试 | 非整个 Electron/系统框架的完整提前翻译；真实应用预热收益尚未量化 |
| 加密扩展 | 保留首轮 SHA-NI/AES/PMULL 路径；完整加密差分通过；真实 Electron SHA/AES 输出与官方 Intel 版一致 | 不外推为 HTTPS 或整应用提速 |
| NEON | 保留首轮 Q32 原生翻译和 QC 累计；饱和乘法与原生参考回归通过 | 未新增全部标量/按元素指令的原生实现 |
| 调试器 | RSP 协议及真实 LLDB 调用/寄存器/退出回归通过 | watchpoint、attach、多线程调试和所有传输方式未全面验收 |
| 其他兼容性 | FP16 累计异常状态；FSQRT 非法输入及 DN；MSR FPCR 强制上下文边界；ISB 后指令发布同步 | 不是全部 FPCR/FPSR 或客体信号语义的完整实现 |
| 直接启动 | 通用 Electron GUI/Node/子进程角色分流；owned universal 与真正 native universal 区分；脚本解释器、argv0、Unicode/空参数、IPC、环境传递；GUI runner exec | 不按应用名称适配；完整 Finder/Dock/窗口交互仍缺验收 |
| 原生库桥接 | 保留首轮 opt-in zlib 桥接；并发及真实 Electron 压缩回归；更新原生进程桥接的架构判断与线程安全 | 未加入更多带状态/回调的第三方 API；Metal 桥接未改动 |

## 验收与真实应用

[21 组总记录](measurements/2026-10-29-round2-acceptance.json)包含全部命令、输出、返回码和二进制哈希。
[CPU 子记录](measurements/2026-10-29-round2-acceptance.cpu.json)包含 10 万规模差分及 NaN/FPCR 矩阵；
FP blocks 零 mismatch、零 fallback。FPCR 实际 ARM fixture 的 20 个用例在解释器、JIT、region 和实际载入的 AOT 下结果一致。

- 官方 Intel VS Code、已安装 ARM VS Code 和 ARM DSH 使用同一独立 Node fixture，SHA、AES、zlib、Worker 线程与自定义 argv0 子进程结果完全一致：[记录](measurements/2026-10-29-round2-electron-node.json)。未加载或修改应用业务模块。
- 最终安装组合的 DSH 与 ARM VS Code 都建立了预期应用目标，`SystemInfo.getInfo` 报告 ANGLE Metal / NVIDIA RTX 3060、硬件合成开启，并拒绝软件 renderer：[DSH](measurements/2026-10-29-round2-dsh-gpu.json)、[VS Code](measurements/2026-10-29-round2-vscode-gpu.json)。这证明启动/GPU 元数据检查通过，不等于窗口呈现或交互验收。
- 两个应用的 ARM 切片与注入前原始备份逐字节一致：[切片校验](measurements/2026-10-29-round2-arm-slices.json)。所有业务程序修改均为零；仅保留通用自动注入的 x86 启动切片。
- 安装后的主 GPU、Metal 和 UI 桥接库与安装前哈希完全一致：[安装清单](measurements/2026-10-29-round2-installed.json)。更新的是翻译器、通用 runner 和原生 exec 桥接。

## 性能取舍

[7 组交替串行采样](measurements/2026-10-29-round2-serial/manifest.json)全部通过原生参考输出检查。
普通 mixed kernels 只有计时输出，不能据此证明数值等价；正确性另由上述差分验收覆盖。

TBI 依赖加载内核观察到耗时减少约 14.95%；短进程 prewarm 观察到约 5.27%，准备成本约 443 ms、合成负载约 7 次启动摊平。
next-use、直接跳转和 region 的结果较小或方向不一致，不宣称一般性收益。
采样时发现后台视频解码服务约占用 6 个 CPU 核心，[负载限制](measurements/2026-10-29-round2-serial/sampling-context.json)已保留；小幅差异不能作为稳定提速/退化结论。
region 继续默认关闭，发布构建配置也维持原样：[LTO 对照与决定](RELEASE-PROFILE-EXPERIMENT.md)。

## 尚未完成的图形验收

本轮 Computer Use 对 DSH 的窗口读取持续超时。恢复此前可用的旧翻译器、旧 exec 桥接和旧 fork/wait runner，等待后端就绪后仍然超时；没有证据把它归因于新 runner 或新翻译器。
VS Code 的 Computer Use 仍明确返回 `Computer Use was not approved to use Code`。未绕过这些工具边界，也未通过另一种 UI 自动化技术代替。
因此本轮不能宣称完整窗口交互、任意新应用免调试可用，或与官方 Rosetta 性能等效。

可重复运行 `tests/attesor/accept-all.py`、`bench/serial-acceptance.py` 和 [Electron 诊断工具](ELECTRON-PROBE.md)。
