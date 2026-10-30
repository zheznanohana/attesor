# integ1 集成验证

2026-10-30；仅本 worktree，顺序执行，无 GUI。**最终所列检查无未解决的代码失败**；平台受限项单列，不冒充通过。

最终 release binary SHA-256：`0b63b7fe4abf2cc4f78aac51ad0af7801c21dbb3db0595b70ea5627d8e8cf178`。默认功能开关、阈值和默认 commpage / getpagesize 的 4 KiB 行为保持不变。
未 stage、commit、push、安装或修改用户应用；保留开始时已有的 staged 内容。

## 结果总表

| 检查 | 实际结果 |
|---|---|
| release build | 通过；编译器仍有 warnings，不冒充零警告 |
| L1 配置 | 无 ON token + 27 个各自单独 ON + 全部 27 个 ON，共 29 配置 |
| 自测 | **580 次请求通过，58,000,000 输入，全部 mismatch=0** |
| workspace cargo | **29 配置均 141 attesor + 9 core 通过**；difftest / doctest 为 0 项 |
| loopra-aot-check | **29 配置 / 209 工作负载通过**；native = interpreter = cold JIT = warm AOT；缓存 identity / OFF 检查通过 |
| headless Node / crypto | **59 项通过**：native 参考 + 29 配置 × Node/crypto；zlib、AES-GCM、子进程 UTF-8 argv0、三个 worker、六项 crypto 结果均核对 |
| COMMON.md 原始 js.js | native / 默认 / 全组合七个内核结果一致，3 项通过 |
| 补充定向 | **20 通过，0 失败，1 skipped**（TCP listener 权限） |
| 公网 HTTPS | 29 配置均 `https error ENOTFOUND`；**未验证**，不声称 TLS/401 通过 |

自测每类请求 100,000 输入；loops 使用 `0x1234 / 0xdeadbeef / 0xc0ffee` 三 seed，其余旧类保留内建固定 seed，jitcode/offline 使用 `0x1234`。
18 类为 `dpimm, dpreg, ldst, branch, blocks, fpblocks, fuse, fuse2, selchain, tbiblocks, fpnan, loops, calls, jitcode, offline, 0E20D400:0E20D400, 0F800000:0F800000, 1F000000:1F000000`。unsupported/skipped 和 interpreter fallback 计数完整保留，不当作 native 通过。
L1 实际执行时间 05:06:12–06:46:22 JST；完整全 ON 是含诊断 token 的交互压力配置，**不是推荐性能配置**。

M/C/A/N/T = 上述自测 / cargo / AOT / Node / 本地 crypto；T 不包括公网 TLS。ported 表示当前树保留生产接入，不表示本轮新移植、完整原始设计或性能目标均已验收。

| feature | token | status ported/skipped | test result |
|---|---|---|---|
| SIMD 高寄存器 | `avx512` | ported | M/C/A/N/T 通过；生产门禁回退，原生 EVEX 未验收 |
| EVEX NaN 修复 | `avx512nan` | ported | M/C/A/N/T 通过；生产门禁回退，原生 EVEX 未验收 |
| 后台翻译 | `bgtrans` | ported | M/C/A/N/T 通过；合用 census 时按既有策略回退 |
| BLR 站点缓存 | `bliccache` | ported | M/C/A/N/T 通过 |
| host call/ret | `callret` | ported | M/C/A/N/T 通过 |
| 快速编码 | `fastenc` | ported | M/C/A/N/T 通过 |
| 循环 NZCV 寄存器 | `flagreg` | ported | M/C/A/N/T 通过 |
| 代码缓存大页 | `hugepages` | ported | M/C/A/N/T 通过；Mach 3 拒绝，普通 mmap 回退通过 |
| 两路 IBTC | `ibtc2` | ported | M/C/A/N/T 通过 |
| BMI2/LZCNT 选指 | `isel2` | ported | M/C/A/N/T 通过；实际 BMI2/LZCNT 均选中 |
| JIT 成本诊断 | `jitcost` | ported | M/C/A/N/T 通过 |
| 循环 CFG 诊断 | `loopcensus` | ported | M/C/A/N/T 通过 |
| 多块循环寄存器分配 | `loopra2` | ported | M/C/A/N/T 通过 |
| dyld fixups | `nativefixups` | ported | M/C/A/N/T 通过 |
| TBI 掩码消除 | `notbi` | ported | M/C/A/N/T 通过 |
| AOT 常量传播 | `offline-constprop` | ported | M/C/A/N/T 通过 |
| AOT 死存储消除 | `offline-dse` | ported | M/C/A/N/T 通过；单独 ON 的 offline 实际 removed_stores=44445 |
| 采样引导 AOT 布局 | `offline-layout-profile` | ported | M/C/A/N/T 通过；真实 profile 排序 / 内容 identity 定向通过 |
| 16 KiB 页语义 | `page16k` | ported | M/C/A/N/T 通过；另有 libc/VM 八场景及故障恢复证明 |
| dyld 路径缓存 | `pathcache` | ported | M/C/A/N/T 通过 |
| 精确失效 / AOT 依赖 | `preciseinvalidate` | ported | M/C/A/N/T 通过；真实跨边界 42→99、迟到缓存拒绝及热 AOT 通过 |
| 后台 QoS | `process-qos` | ported | M/C/A/N/T 通过 |
| write-through 循环缓存 | `regioncache` | ported | M/C/A/N/T 通过 |
| 不可变 rel32 链 | `rel32chain` | ported | M/C/A/N/T 通过 |
| 共享 Plain 翻译 | `sharedtrans` | ported | M/C/A/N/T 通过；文件 backing 跨进程协议通过，真实 POSIX shm 受限 |
| 链槽对齐重定位复用 | `slotreuse` | ported | M/C/A/N/T 通过；全组合覆盖 8/8 prefix |
| 分层 JIT | `tiered` | ported | M/C/A/N/T 通过；jitcode 实际执行冷层完成与晋级 |

## 修复与敏感性证明

- **page16k / Node BRK**：原代码要求 16 KiB mprotect/munmap，却在 commpage 和数字 HW_PAGESIZE 查询报告 4 KiB。仅 opt-in 下统一四个 libc/VM 发现接口；默认、ON、OFF 优先和伪 token × interpreter/JIT 八场景通过。临时恢复旧数字查询后定向检查失败，恢复后通过。
- **preciseinvalidate / 热 AOT 被无关映射全部撤销**：将此 opt-in 的所有 AOT reader 请求限制到 executable 源范围，登记保守完整来源及最多 1,024 条合并失效记录；相交、加载前及迟到旧缓存仍拒绝。旧无证明入口仍保留全局禁用 latch，不反转 IMAGES/BLOCKS 锁序。loaded 计数仅统计实际发布入口。`TRANSLATOR_VERSION` **64 → 65**，磁盘格式不变。
- **真实边界证明**：邻接代码由 42 改为 99、失效后保留无关 AOT 并取得 99；临时放开 reader 边界实际取得旧值 42，检查失败。恢复后 disk-AOT 自测通过。反复改写页的 offline 夹具明确隔离合成图像生命周期，不放宽生产规则、不将 JIT 回退当作 AOT。
- **自测交互与规模**：jitcode 使用实际冷层完成/晋级，再检查原始公开入口的 8 种 prefix；每次失效后仅清理该独立夹具的过期链槽元数据，消除二次方扫描。非内存编码域以精确 OR 零缓冲区校验替代冗余复制，仍逐字节覆盖整个缓冲区并断言解释器不写内存；内存域保留完整快照，输入分布与数量不减。旧类 mismatch 现在非零退出。
- **组合敏感性**：破坏 continuation rebase / probe 寄存器，全 ON 的 256 例分别 mismatch=246 / 256 且非零退出；恢复后三 seed 均 mismatch=0。所有源码变异均恢复，无生产故障开关残留。
- **headless 夹具**：子进程 census stderr 原先触发 spawnSync 1 MiB ENOBUFS，改为交给 harness 流式收集。全组合 worker 超过首轮 180 秒预算，扩至有界 600 秒后完成；未减少工作或忽略断言。原始 js.js 的七项数值也与 native 独立核对。

## 定向与平台边界

- 默认 / 全 ON 的 RSP 与 LLDB inherited-FD 均通过；TCP `--gdb` listener 返回 EPERM，记 skipped，未提升权限。
- weighted-layout、offline DSE+constprop 共存、QoS、nativefixups 全页 shadow、native DSP/vImage/compression/crypto、ImageIO、zlib、regioncache、icache handoff、FP16/FPSR/FPCR、crypto KAT 与 profile-report 检查通过。native bridge 检查包含实际调用计数、字节或数值 oracle；不等同于 GUI/GPU 验收。
- 生产 AVX-512 四项 sysctl 均 EPERM；实际 flags `0x8000303e` 的 AVX-512 位未选中，BMI2/LZCNT 均选中。AVX-512 压力 / AOT 只证明安全回退，**不证明原生高寄存器执行**。
- 真实命名 POSIX `shm_open` 探针 EPERM；cfg(test) 私有文件 backing 的独立 producer/consumer 协议及真实执行通过，不能冒充生产命名 shm 已验收。
- 真实 2 MiB backing 未获批（Mach 3）；fallback 已测试。GUI、Metal/VideoToolbox、L2 和所有模块的广泛性能 / 默认开启验收未做。
- preciseinvalidate 的来源仍是保守凸包，shared cache 使用整个登记范围；墓碑超过预算会拒绝后续 AOT 发布，可能保守降级。
- 全组合中 bgtrans 因 census、sharedtrans 因 tiered / probes 等按既有策略回退；单独 ON、cargo 定向及真实 profile / 优化工作检查另行覆盖。census observer 可使全组合 DSE removed_stores=0，不冒充优化工作量。

## 有界微测（不是模块全面提速结论）

预热与测量使用一致的 STATS identity，并要求真正 loaded AOT / loop_regions；预热不匹配的试跑已废弃。
同一最终 binary，交替 OFF/ON，校验相同输出；Node pilot 不计入正式样本。off/on > 1 表示 ON 更快。

| 工作负载 | OFF ms | ON ms | paired off/on 中位数 |
|---|---|---|---|
| preciseinvalidate，warm-AOT 内核，3 对 | [49.414, 48.092, 47.274] | [48.577, 48.372, 51.094] | 0.9942× |
| page16k，headless Node 启动，3 对 | [1271.833, 1280.389, 1298.633] | [1348.514, 1297.392, 1326.022] | 0.9793× |
| page16k，扩展 9 对 | 见原始 JSON | 见原始 JSON | 1.0070× |

page16k 的 3 对小样本为约 2% 较慢，扩展 9 对为约 0.7% 较快；未见稳定回归，也不据此宣称提速。
preciseinvalidate 微测近似持平。负载记录：`7:06  2 users, load averages: 1.98 2.09 2.42`。

## 复跑与证据

```sh
TMPDIR=/private/tmp cargo build --release -p attesor
python3 tests/attesor/integration-check.py --cargo --aot --resume
python3 tests/attesor/integration-headless-check.py --output target/integ1/headless-final
python3 tests/attesor/integration-headless-check.py --js-only --configs default all --output target/integ1/js
python3 tests/attesor/page16k-check.py
```

完整组合由 `integration-check.py` 的 27 项 TOKENS 精确逗号拼接；每条命令的 ON/OFF、退出码、SHA、耗时和计数均写入 JSON。
主要证据：`target/integ1/l1/summary.json`、`headless-final/summary.json`、`directed/summary.json`、`js/summary.json`、`perf/summary.json`、`page16k-perf-extended/summary.json`。
故障证明与恢复：`closed-aot-proof-final.log`、`page16k-proof-final.log`、`jitcode-proof-final.log` 及对应目录；环境探针 `host-policy.log`。
所有这些路径均在 `target/integ1/` 下。超过 5 MiB 的 trace 已压缩为 `.log.gz`；私有大缓存和重复 dscslide/AOT 副本已删除，保留结果、原始测量、补丁和提交说明。
提交说明：`target/integ1-verify-commit-message.txt`；本轮增量补丁：`target/integ1-verify.patch`。
