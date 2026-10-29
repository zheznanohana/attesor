# 八个专项的首轮集成验收（2026-10-29）

八个专项各由独立 agent 开发，主分支统一合并、复测。首轮 **11 个自动化检查组全部通过**，
安装脚本也通过临时 HOME/PREFIX 安装检查。已更新 `~/bin/attesor`、桥接库和启动器；
已安装模拟器的 SHA-256 与验收二进制完全一致。原生库桥接保留显式开关。

这份验收覆盖本轮交付，不代表整个优化目标已经完成。官方 Rosetta 未作为本机实测对照；
下面比较的是同一 Intel 主机的旧版/新版，或同一新版的桥接关闭/开启。

## 八项交付

| 方向 | 已集成与复核的范围 | 仍需推进 |
|---|---|---|
| 1. JIT 代码质量 | 修复 NaN 符号/载荷、sNaN 优先级及 FMA 特例，减少 FMA 普通路径复制和跳转；10 万 FP blocks 零差异、零回退；NaN 矩阵默认/FPCR 各 62,208 条通过 | 跨基本块寄存器、循环与间接调用开销；完整 FPSR 状态 |
| 2. 提前翻译与缓存 | 线程尾部、旧版本/特性隔离、构建中断重试、并行 builder、fork 与失败 exec；7 项进程回归证明强退后实际载入缓存 | 整个应用的后台预翻译和真实启动收益 |
| 3. 加密扩展 | SHA256SU0/SU1 增加 SHA-NI；12 万例 AES/PMULL/SHA 差分及 FIPS 已知答案通过 | HTTPS/真实应用端到端计时与其他加密家族 |
| 4. NEON | 32 位向量 SQDMULH/SQRDMULH 原生路径，16/32 位 QC 累计状态；28,800 次独立参考测试及两类各 2 万原生差分通过 | 标量/按元素形式及新热点采样 |
| 5. 调试器 | 49 项真实 RSP 检查；LLDB 符号、表达式、客体函数调用、寄存器修改、finish 与退出通过 | 多线程、watchpoint、attach、自动库符号；当前环境未验证 TCP 监听 |
| 6. 其他兼容性 | 半精度转换四舍入模式、FZ/DN/AHP；独立整数参考/JIT/解释器 60,000 次一致，额外 tiny 边界及 4 万原生差分通过 | 硬件视频、更多 GUI/Metal 应用；完整浮点异常状态 |
| 7. 直接启动 | 保留 Claude 的 universal stub；原子注入/恢复、迁移、备份隔离、更新、参数和退出码；25 项 fixture 通过，包含真实 ARM 执行 | 真实 Finder/Dock 外观与更多应用更新流程 |
| 8. 原生库桥接 | zlib 的 CRC32/Adler32/compress2/uncompress；错误码、长度、tag 指针、并发和 ARM→原生→ARM 链通过；启用计数 4,332，关闭 0 | 应用实际收益、更多 API 和带回调/状态的接口 |

并发测试还暴露了已有的线程初始化竞争：旧版在独立创建/回收测试中出现 SIGSEGV。
现在创建调用等待 Mach port 初始化后再返回；10 个独立进程共 1,200 个立即返回的线程全部
正确 join。桥接与该修复组合后的完整并发测试已复核通过。

## 串行性能

计时与正确性测试分开运行。所有微基准校验和另外与原生 x86 参考比对一致。
SHA 使用 7 组交替样本，其余主要负载 5 组；冷缓存记录测试 3 组。

| 测试 | 之前/关闭 | 之后/开启 | 耗时减少 |
|---|---:|---:|---:|
| 浮点 n-body 内核 | 145.1 ms | 134.6 ms | 7.2% |
| SHA-256 消息调度内核 | 18.404 ms | 6.698 ms | 63.6% |
| NEON Q32 负载（整进程） | 640.493 ms | 584.866 ms | 8.7% |
| CRC32，累计 512 MiB | 115.810 ms | 33.453 ms | 71.1% |
| Adler32，累计 512 MiB | 1379.961 ms | 35.686 ms | 97.4% |
| zlib 压缩，16 MiB | 286.409 ms | 49.422 ms | 82.7% |
| zlib 解压，16 MiB | 61.593 ms | 14.899 ms | 75.8% |
| zlib 测试整进程 | 2460.050 ms | 942.520 ms | 61.7% |

混合 CPU 基准整进程只减少约 1%，不视为一般应用提速证明。启用记录、每次使用空缓存且关闭
后台构建时，整进程 3434.114 → 3439.914 ms（约慢 0.17%），未观察到明显记录开销；
它不等同于 Electron 的启动测试。专项负载的收益也不能外推到所有应用。

原始采样和二进制哈希：
[CPU](measurements/2026-10-29-team-kernels.json)、
[SHA](measurements/2026-10-29-team-sha.json)、
[NEON](measurements/2026-10-29-team-neon.json)、
[zlib](measurements/2026-10-29-team-native.json)、
[冷缓存记录](measurements/2026-10-29-team-cold-recording.json)。

## 复现总验收

```sh
cargo build --release
python3 tests/attesor/accept-all.py --attesor target/release/attesor \
  --output /tmp/attesor-acceptance.json
```

11 个检查组包括 workspace unit、CPU、crypto、AOT、NEON、FP16、RSP、LLDB、原生 zlib、
线程启动和启动器。完整命令、输出、返回码和被测二进制哈希保存在
[总报告](measurements/2026-10-29-team-final-acceptance.json)及
[CPU 子报告](measurements/2026-10-29-team-final-acceptance.cpu.json)。

安装后另外验证了桥接库签名和默认路径发现，安装目录中的模拟器继续通过标量结果与 zlib
校验和检查。安装使用 `NO_LAUNCH_AGENT=1`，没有重新注册登录 agent。

## 当前证据边界与下一轮

真实 Finder/Dock 图标尚未做视觉验收。当前执行环境中，原生与客体的 VideoToolbox 都返回
`-12908`，所以本轮没有硬件视频兼容性结论。大型应用启动收益、整应用预翻译和剩余 JIT
瓶颈仍未解决；下一轮继续这些方向，目标保持进行中。
