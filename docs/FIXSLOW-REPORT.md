# FIXSLOW：process track 修复与验收

2026-10-30（JST），CPU-only，x86_64 Darwin 25.6.0。

## 结论（不冒称全部 Done）

两个 opt-in token 均保留、默认关闭，OFF 优先；未执行 git 写操作。
**原生热点已大幅修复，但用户的严格全部验收条件尚未完全达到，不能批准默认开启。**
- Electron Framework：652,675 rebases / 8,194 binds。最终实现首次处理均值 **8.770 ms**，未预先触碰目标页的 guest loop **19.804 ms**；五对中每对原生均更快。热页原生 plan+VM+write 均值 **1.849 ms**，达到理想的约 1–2 ms。
- **首次处理仍未达到绝对 <7.5 ms**；不能用热页的 1.85 ms 隐藏首次触页/COW 成本。
- QoS 的旧约 4 倍 prewarm 退化已去掉；最终九对中 prewarm/前台延迟的 ON 均值仍约 +2%，区间跨零。未检出显著回退不等于证明零回退，因此严格门槛仍待证。
- 未证明任何一项“原理上必慢”，故**没有丢弃功能或删除 token**。下述未达项不是不可修复的结论。

## 实现与阶段定位

- `nativefixups.rs`：正常模式不再分配全页快照、dirty bitmap、BTreeMap 节点或逐 slot 的 run/value 元数据。只保留每个 chain page 的位置/起点/预计算 delta，以及一次复制的最终 bind 表。
- 先解析整个输入、校验 segment/overlap，再按实际 host VM region 合并 R/W 检查；随后校验全部链、reserved bits 和 ordinal，**全部成功后才写任何指针**。失败仍执行原 guest fallback。
- 直接在映射页上进行等步长四项批处理；稠密前缀用 runtime-length、允许未对齐的简单循环。`otool` 确认生成 `pand/psllq/paddq/movdqu`；证据 `target/fixslow/apply-page-dense-disassembly.txt`。没有逐指针 syscall、锁或 heap allocation，只写实际 slots，保留 gap/其他数据字节。
- 大于等于 128 个 chain pages 才分块，最多 4 个 scoped host workers，每个 256 KiB stack；校验和写入分别 join，失败创建线程时串行处理对应块；没有跨 fork 的持久线程池/队列。小映像串行。阶段计时包括线程启动与 join。
- UUID/symbol/完整函数 hash pin、格式 2/6、只在本进程 COW text 安装 hook、只写非代码 R/W 页等边界不变。保留 `VERIFY=1` 的实际 writer→全页比较→只恢复 encoded slots→实际 guest dyld→全页比较。
- 新增 `ATTESOR_NATIVEFIXUPS_VERIFY=guest-first` 逆序控制；`ATTESOR_NATIVEFIXUPS_GUEST_TIMING=1` 只计原 guest，不先 plan/snapshot/访问目标页。两种诊断勿同时设置，均须显式 ON。
- `process_qos.rs`：用户等待的 `--prewarm` 在检查 token 之前直接返回，**不调用 QoS API，也不为了诊断读取策略**。只有 detached exit/exec-save builder 才 best-effort cap 到 default `0x15`，不提升已有更低优先级。EPERM 时保留继承策略，不再使用 `darwin_bg` 回退。同步 save、失败 exec 返回路径及 guest/UI 线程不降级。
- 没有修改 translator/JIT 或 hook 指令，`TRANSLATOR_VERSION=54` 不变；完整 flag/build cache identity 隔离保留。

## 测量方法与原始数

`cargo build --release`：现有 `[profile.release] debug=1,opt-level=3`，构建日志为 `optimized + debuginfo`。只用 `Instant` 分阶段，未用 sample/Instruments。`plan_ns` 不含单列 `vm_ns`，总和加 `write_ns`；不是隔离 CPU 时间。

相邻 OFF/ON 对、奇数轮反转次序，保留全部样本及逐次 uptime。私有 DSC 已 warm，普通 guest `--no-aot`、checkpoint/exit builders 关闭；prewarm 每个 job 独立冷私有 AOT cache，完成后清理。前台 proxy 是 headless Electron Node JS 的 launch-to-exit，不是 GUI 响应；response 的 12 个串行 builder 覆盖前台运行，工具断言确实重叠并等待所有 builder。

原始记录、binary/source SHA256、uptime：
[2026-10-30-fixslow.json](measurements/2026-10-30-fixslow.json)。完整日志/命令/私有 CLI 留在 `target/fixslow/`；大型 DSC/AOT 副本已清理。起始树已有候选实现和测量，续接而非覆盖；`COMMON.md` 的既有改动未改动。

### Native：同输入验证与真正首次处理分开

历史报告：native 54.713 ms / guest 7.474 ms。不能直接把历史不同负载的数字当本轮 speedup。
本轮 baseline snapshot 与 HEAD 源文件一致；保留的 before binary 为 phase-timing 版，SHA256 `f1794253ada94fccd659f49d08e0077135e44f91ef7c29bb6cd0cb4a84729a30`；最终 binary `c68f2f8f8a7772ca7cf3f7ba1091d334fd3b25b5429f733eb6396bb1e0814946`。

| 条件 | 全部 native ms | 全部 guest ms |
|---|---|---|
| 原实现，native 先执行，3 次验证 | 34.011, 32.917, 37.889 | 5.940, 4.670, 5.801 |
| 最终实现，native 先执行，3 次验证 | 9.949, 9.931, 13.274 | 4.598, 4.821, 8.204 |
| 最终实现，各自首次处理，5 对独立进程 | 8.655, 9.026, 9.351, 9.102, 7.718 | 15.110, 19.915, 33.419, 16.544, 14.032 |
| 最终实现，guest 先执行后恢复同一 encoded input | 2.094, 1.838, 1.847, 1.951, 1.515 | 12.783, 11.997, 13.031, 10.858, 10.757 |

验证顺序使第二个 writer 不再付首次写入成本。表末行 native 是热页，guest 是先写；**不是拿这行两数做公平提速比**。公平首次处理来自“不先碰目标页”的独立 guest-timed 对照。页驻留、COW、线程启动/调度一起影响经过时间，没有把差额全归因 CPU 或某一种缺页。

| 原生阶段（ms，全部原始数） | plan（不含 VM） | VM prechecks | writes |
|---|---|---|---|
| 原实现，3 次验证 | 19.711, 18.838, 21.063 | 0.016, 0.020, 0.020 | 14.283, 14.058, 16.806 |
| 最终，5 次首次处理 | 2.194, 2.243, 2.343, 2.085, 2.031 | 0.011, 0.009, 0.018, 0.008, 0.008 | 6.450, 6.775, 6.989, 7.008, 5.679 |
| 最终，5 次热页处理 | 1.270, 1.065, 1.053, 0.936, 0.895 | 0.013, 0.015, 0.012, 0.014, 0.010 | 0.810, 0.759, 0.782, 1.002, 0.611 |

原实现的 snapshot/dirty 管理和解析是大项，VM 查询本身不是主要成本。最终热页成本低很多；不能据此删掉必需校验或把首次处理成本移出计时。

### Native：launch-to-exit（全部 3 对，秒）

| workload | OFF | ON | ON/OFF 均值 |
|---|---|---|---|
| trivial | 0.192, 0.278, 0.173 | 0.263, 0.242, 0.188 | 1.077 |
| node | 3.014, 2.073, 1.751 | 3.470, 1.696, 1.675 | 1.000 |
| foundation | 0.682, 0.264, 0.246 | 0.682, 0.268, 0.246 | 1.004 |
| big | 0.346, 0.170, 0.163 | 0.334, 0.172, 0.160 | 0.981 |

trivial 不执行 fixup loop，是负控制。高负载样本及这些均值 >1 的行都保留；它们不支持“所有启动场景均不变慢”的宣称。另有交错 before/after crypto launch 的完整原始数在 final3-ab，不能用其中有利的 ratio 替代上表。

### QoS 机制、候选与最终复测

`PRIO_DARWIN_BG` 把调度优先级降到最低，并节流 disk/network I/O，不能当成 pthread QoS 失败时的等价替代。[Apple setpriority(2)](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/setpriority.2.html)
QoS background/utility 也包含性能、能耗和 I/O 调度权衡，不只是“让出一点 CPU”。[Apple QoS 指南](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/EnergyGuide-iOS/PrioritizeWorkWithQoS.html)
旧 prewarm 实际为 QoS=0x09、darwin_bg=0；旧 detached builder 才因 EPERM 回退 bg=1。不能把两条路径都说成同时启用了两种策略，也没有单独隔离 CPU 频率和 I/O 的各自贡献。

| 候选 / prewarm | OFF 全部秒数 | ON 全部秒数 | 均值 ratio |
|---|---|---|---|
| 旧 0x09 / BG fallback | 0.248, 0.234, 0.234, 0.239, 0.237 | 0.971, 0.976, 0.956, 0.946, 1.003 | 4.068 |
| utility 0x11，无 BG | 0.285, 0.292, 0.303 | 0.282, 0.304, 0.551 | 1.292 |
| default 0x15，无 BG（仍降级 prewarm） | 0.247, 0.254, 0.288 | 0.250, 0.305, 0.284 | 1.063 |

| utility 0x11 的其他指标（3 对，秒） | OFF | ON | ratio |
|---|---|---|---|
| contention builder | 0.283, 0.353, 0.362 | 0.305, 0.478, 0.479 | 1.264 |
| foreground JS latency | 2.051, 2.266, 2.550 | 2.158, 2.126, 2.197 | 0.944 |
| 12 builders 合计 | 3.655, 3.706, 3.812 | 3.807, 3.752, 3.829 | 1.019 |

utility 的三对 contention builder 均值也退化，故没有停在“换一个常量”上；这些候选日志保留。最终把 awaited prewarm 排除出后台策略。实测 detached builder：`qos_result=1 qos=0x21 darwin_result=0 darwin_bg=0`，发布 AOT 后退出；**这条真实路径因 EPERM 没有成功改变 QoS，不宣称已降低其优先级或提高了响应性**。

最终独立九对验证使用默认 OFF（不设 token）；早期五对显式 OFF 数据也完整保存。下面列出全部九次秒数：

| workload / 指标 | OFF | ON | ratio |
|---|---|---|---|
| prewarm wall | 0.259, 0.257, 0.289, 0.273, 0.270, 0.256, 0.271, 0.265, 0.261 | 0.262, 0.260, 0.291, 0.312, 0.265, 0.271, 0.268, 0.262, 0.259 | 1.021 |
| contention 总 wall | 1.329, 1.453, 1.578, 1.427, 1.437, 1.432, 1.398, 1.400, 1.326 | 1.345, 1.387, 1.473, 1.434, 1.386, 1.362, 1.420, 1.379, 1.343 | 0.980 |
| contention builder | 0.279, 0.286, 0.330, 0.292, 0.262, 0.274, 0.282, 0.277, 0.276 | 0.289, 0.271, 0.306, 0.292, 0.273, 0.285, 0.284, 0.268, 0.283 | 0.997 |
| 持续 builder 下前台 JS latency | 2.091, 2.471, 2.272, 2.108, 2.213, 2.227, 2.265, 2.160, 2.153 | 2.083, 2.359, 2.240, 2.515, 2.107, 2.370, 2.173, 2.210, 2.308 | 1.020 |
| 12 builders 合计 | 3.320, 3.640, 3.399, 3.244, 3.286, 3.371, 3.392, 3.294, 3.271 | 3.243, 3.486, 3.506, 3.571, 3.287, 3.497, 3.358, 3.330, 3.457 | 1.017 |
| response 总完成时间 | 3.332, 3.658, 3.411, 3.255, 3.297, 3.393, 3.403, 3.306, 3.283 | 3.254, 3.503, 3.518, 3.585, 3.298, 3.511, 3.370, 3.341, 3.469 | 1.017 |

最终 paired ON−OFF 95% t 区间（df=8，描述性、非独立复现保证）：

| 指标 | OFF 均值 s | ON 均值 s | 差值区间 ms |
|---|---|---|---|
| prewarm | 0.266786 | 0.272267 | [-5.0, 16.0] |
| contention | 1.420051 | 1.392102 | [-63.7, 7.8] |
| foreground latency | 2.217730 | 2.262696 | [-84.9, 174.8] |

区间跨零不是零回退的证明，不把它记成提速或严格验收通过。旧报告的 contention wall 1.482x 本轮 baseline 未稳定复现；但旧 builder 的独立完成时间仍约 4x，原始记录保留。
CPU 为 `RUSAGE_CHILDREN` 所有等待子进程（含其线程）的累计 user+system，不是翻译 CPU；全部原值在 JSON。

## 实际检查与剩余边界

```text
cargo build --release                                  PASS (optimized + debuginfo; warnings)
cargo test --release --workspace -- --test-threads=1   OFF/native/qos/combined 各 82 + 9 passed, 0 failed
nativefixups directed tests                            12 passed
process_qos directed tests                             4 passed
13 selftest classes × 100000, OFF/native/qos/combined   所有 mismatch=0（保留 skipped）
loops × 20000 × {1,0xdeadbeef,0x100f5e1f7e57cafe}        四配置 mismatch=0
loopra-aot-check.sh                                    四配置 × 7 workloads，native=interp=JIT=AOT，identity checks PASS
gdb-protocol.py / gdb-lldb-fd.py                        combined PASS（继承 socketpair，无 TCP listener）
nativefixups-check.py                                  PASS 全页、16KB、interp、大 dylib reopen、真实 AOT、6 children、unknown UUID、OFF 优先
process-qos-check.py                                   PASS 同步/失败 exec 不降级、实际 detached builder 发布后退出、payload 相同
JS OFF/native/qos/combined                             7 个返回值逐项相同
故意破坏 writer rebase delta                          6 failed / 6 passed, exit=101；立即还原后 workspace 82+9 PASS
Python py_compile / git diff --check                   PASS
```

- 全部阶段/端到端数据没有删异常值；负载明显变化，不能把单个最好样本或噪声均值当总启动提速。
- 原有 loader-lock 生命周期假设不变：目标 image 不被并发 unmap/mprotect；并行 worker 不提供恶意 guest VM 竞态事务。创建失败串行回退有实现，未人为耗尽系统线程资源测试。
- 仍只覆盖已审计 dyld UUID/ABI 和支持的 chain formats。未知输入保守回 guest；没有 closure persistence、zygote 或 live shared translations。
- CPU-only：没有跑 GUI/GPU/Metal、网络 TLS；不借历史 TLS 结果宣称本轮通过。
- **严格剩余项**：首次 native <7.5 ms 尚未稳定达到；QoS 及负控制的全部 workload 零回退也未被这些噪声数据证明。保留 opt-in，不默认打开、不删除，没有把剩余问题宣布成原理上不可解决。

## 复现与交付

```sh
TMPDIR=/private/tmp cargo build --release
python3 tests/attesor/nativefixups-check.py --output target/fixslow-repro-native
python3 tests/attesor/process-qos-check.py
python3 tools/fixslow-bench.py --binary after=target/release/attesor --output target/fixslow-repro-loop --pairs 5 --workloads fixups-cold fixups-guest-first
python3 tools/fixslow-bench.py --binary after=target/release/attesor --output target/fixslow-repro-qos --pairs 9 --default-off --workloads prewarm contention response
python3 tools/fixslow-check.py --attesor target/release/attesor --output target/fixslow-repro-qa
```

实际执行的 QA 命令是 `target/fixslow/regression.py`，使用保存的 `attesor-final`；其流程已整理成上述可复用 `tools/fixslow-check.py`（CLI/py_compile 检查通过）。wrapper 保证被 loopra 脚本清理环境后仍实际带 token。完整命令/日志在 `target/fixslow/`，大型私有缓存已清理。rustfmt component 未安装，未安装新工具链。

提交说明：`target/fixslow-commit-message.txt`；补丁：`target/fixslow.patch`（不包含用户既有 COMMON 改动）。没有 git commit/index/reset/push、用户缓存、应用或其他用户进程写操作。
