# Track C：运行时生成代码测量与优化报告

日期：2026-10-29；分支：`jitcode`；工作树：`/Users/USER/attesor-p2c`。

**结论：先修翻译器的重复工作，而不是扩大失效缓存。**
本负载的运行时/匿名代码翻译占活跃样本约 2.63%，不是主因；镜像翻译/装载与已翻译代码执行占约 39.18% / 32.01%。
新增 `ATTESOR_JIT_ON=slotreuse`，避免链槽对齐时重复生成和汇编整个单元；默认关闭，等待 Claude 独立测量和接受。
新增诊断开关 `ATTESOR_JIT_ON=jitcost`，与优化独立。最终 `TRANSLATOR_VERSION=55`。

## 1. 最重要交付：成本拆分表

以下为**最终 v55、三轮 before/after 交替运行**的累计线程采样。
`before=jitcost`，`after=jitcost,slotreuse`；采样目标周期 1 ms。
百分比分母排除 idle：before 26,166、after 22,755 个活跃样本；idle 另为 103,815 / 93,442。
这是整个 headless Node 进程（**含 dyld/Electron/Node 启动**），不是只截取七段 JS 内循环。
线程时间可以重叠，不能把采样数或下列事件时间相加当成进程墙钟时间。

| 成本 | before 样本 | before 活跃占比 | after 样本 | after 活跃占比 | 独立事件计时：before → after，均值 ms/次进程 |
|---|---:|---:|---:|---:|---|
| 镜像翻译 / AOT 装载 | 10,251 | 39.18% | 6,979 | 30.67% | 3,404.886 → 2,264.115 |
| 运行时/匿名代码翻译 | 688 | 2.63% | 416 | 1.83% | 231.203 → 135.778 |
| 执行已翻译代码 | 8,377 | 32.01% | 8,268 | 36.33% | 仅采样，不伪造精确计时 |
| Rust dispatcher / 查表 / 链接 | 2,381 | 9.10% | 2,539 | 11.16% | 仅采样，不伪造精确计时 |
| 非 RET 的间接目标查表及转移 | 91 | 0.35% | 97 | 0.43% | 仅采样，不伪造精确计时 |
| 调用设置 / 返回（含 RET 查表） | 254 | 0.97% | 208 | 0.91% | 仅采样，不伪造精确计时 |
| 失效处理 | 29 | 0.11% | 17 | 0.07% | 10.761 → 6.420 |
| 冷代码 / helper 解释执行 | 1,144 | 4.37% | 1,403 | 6.17% | 仅采样，不伪造精确计时 |
| BSD syscall | 2,922 | 11.17% | 2,803 | 12.32% | 仅采样，不伪造精确计时 |
| Mach syscall | 29 | 0.11% | 25 | 0.11% | 仅采样，不伪造精确计时 |

说明：
- `translate-runtime` 按“不属于已登记镜像的 PC”划分，不能等同于“已证明全部来自 V8”；包括其它匿名代码。
- 翻译计时为调用者 elapsed thread wall time，含翻译锁等待；`translate` 采样还包括 AOT 装载，二者不是完全相同的口径。
- 间接转移保留 probe 状态直至目标块入口，以免把目标加载/分支误预测全部误算成 guest 执行；RET 的查表归在 call-return，避免重复计算。
- probes 添加入口 MOV、phase MOV 和单写线程的计数 INC，会扰动执行；这些占比不是零开销硬件 perf 测量。无 probes 的墙钟 A/B 单列于第 4 节。
- 事件 ns 为嵌套计时：emit、assemble、alignment、retranslate 已包含在翻译时间中，不能再次相加。

**整体 top 3：** ① 镜像翻译/装载 39.18%；② 已翻译代码执行 32.01%；③ syscall 合计 11.28%（主要是启动时的 shared-region 映射）。
在要求拆分的 JIT 项目中，第 3 项是 dispatcher 9.10%。运行时翻译 2.63%、间接转移及 call/return 合计 1.32%、失效 0.11%。
因此本次针对最大的可直接修复子成本——翻译对齐重做——同时消除多余 emit 和 assemble；没有凭猜测引入 hash/W^X 跳过或新的 BLR cache。

### 翻译器内部工作量与事件时间

| 项目 | before 三轮原始值 | after 三轮原始值 |
|---|---|---|
| 镜像翻译 ms | 3300.982 / 3311.838 / 3601.840 | 1965.117 / 2038.178 / 2789.049 |
| 运行时/匿名翻译 ms | 240.583 / 221.852 / 231.174 | 119.432 / 131.995 / 155.906 |
| 对齐修复阶段 ms | 987.705 / 1050.930 / 1085.108 | 3.202 / 3.471 / 3.833 |
| 需要对齐修复的单元数 | 68,567 / 68,106 / 67,506 | 70,763 / 70,909 / 72,349 |
| 复用首次结果的单元数 | 0 / 0 / 0 | 70,763 / 70,909 / 72,349 |
| 运行时/匿名翻译单元数 | 4,813 / 4,824 / 4,790 | 4,890 / 4,928 / 4,949 |

计数键 `alignment-retries` 表示“需要修复的机会数”；after 不再真正重试。
before 共 243,381 个单元、204,179 次对齐重做，即 emit/assemble 各 **447,560 次（1.839 次/单元）**；
after 254,884 个单元、214,021 个需要修复的单元，emit/assemble 各只需 **254,884 次（1 次/单元）**。
这是与机器负载无关的编译工作量下降，不是声称执行时指令数下降。
翻译事件均值合计 3636.090 → 2399.892 ms（after/before 0.660）；按单元归一化 44.820 → 28.247 μs。
V8 的并发分层/GC 调度使每次生成的代码与调用次数不同，不能只比较未归一化的总量。

## 2. 失效、字节数与随后的重翻译

下表均为三次进程的**累计值**；bytes 是求和，不是互不重叠的唯一字节数。

| 项目 | before | after |
|---|---:|---:|
| IC IVAU 次数 | 5,643 | 5,851 |
| IC IVAU 请求字节 | 361,152 | 374,464 |
| mprotect(+EXEC) 次数 | 3 | 3 |
| mprotect(+EXEC) 请求字节 | 805,306,368 | 805,306,368 |
| mprotect 实际移除单元数 | 0 | 0 |
| 其它失效入口次数 | 0 | 0 |
| 实际移除单元数（此处全部由 IC IVAU 触发） | 243 | 260 |
| 移除单元的 host code bytes | 56,277 | 58,488 |
| 随后同 entry 重翻译单元数 | 84 | 51 |
| 这些重翻译的 host code bytes | 19,381 | 11,210 |
| 间接目标查表次数（含 RET） | 153,390,427 | 146,429,346 |
| IBTC miss 次数 | 889,345 | 934,416 |
| guest 调用数 | 111,718,682 | 105,147,089 |
| guest 返回数 | 112,178,555 | 105,631,880 |

- 每轮唯一一次 mprotect(+EXEC) 请求为 268,435,456 bytes，但没有移除任何已有单元；不能把它说成“256 MiB 被重新翻译”。
- IBTC miss 比率 0.580% → 0.638%；未新增 IBTC 优化，也不把这个波动解释成优化收益。
- 同 entry 重翻译耗时均值 0.988 → 0.436 ms；这是已经发生失效的 entry 再次编译的计时，不包含其它 entry 的间接连带工作，也不证明字节一定有变化。
- 原有保守 guest 范围与失效语义保持不变；没有根据这组很小的失效成本放松正确性检查。

## 3. 实际改动

1. profiler 增加 runtime/image 翻译状态、失效状态、可选间接分支/call-return 状态；增加来源、请求字节、移除 host bytes、同 entry 重翻译、分支次数及 IBTC miss 计数。
   v2 完整 merger records 增加 `jit-metric`，保留 v1/v2 读取兼容；`tools/profile-report.py` 合并指标并显示排除 idle 的占比。
   ns 计数使用原子变量，不引入可在 fork 时遗留的新指标锁；线程结束累计计数，fork 子进程重置，exec 保留前一 epoch 的记录。
2. `slotreuse`：首次汇编发现链槽不对齐时，前置 1–7 字节未执行空间，整体移动该单元，复用首次机器码。
   单元内相对跳转及 RIP-relative 常量/槽位位移不变；初始化 slot 和 continuation 的 host 地址统一加 prefix。
   检查没有需重新定位的外部分支 relocation，并断言槽位 8 字节对齐。
3. 新增 `--selftest-jit jitcode N SEED`：覆盖整数/FP、RIP-relative pool、cold paths、条件边、BL/BLR、RET、冷/热链、失效重编译和全部八种 prefix 对齐。
   更新 loopra AOT 检查脚本，保留调用者显式传入的 `ATTESOR_JIT_ON`，避免“带开关测试”实际上把开关清掉。

## 4. 原始 A/B 数字（不隐藏回退）

每种系列独立 before→after 交替三轮。所有 JS 校验值均匹配原 helper。
墙钟列含启动，单位 s；其余为 helper 的原始 ms。

| 系列/轮次/配置 | 墙钟 s | loops | objects | json | regex | strings | closures | sort |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| plain-1-before | 5.616 | 518 | 708 | 211 | 242 | 109 | 302 | 573 |
| plain-1-after | 4.369 | 455 | 503 | 217 | 209 | 72 | 241 | 531 |
| plain-2-before | 7.932 | 534 | 1648 | 533 | 356 | 107 | 398 | 570 |
| plain-2-after | 4.322 | 419 | 507 | 196 | 209 | 69 | 209 | 701 |
| plain-3-before | 5.717 | 718 | 640 | 227 | 244 | 102 | 246 | 749 |
| plain-3-after | 4.862 | 490 | 694 | 179 | 212 | 63 | 300 | 524 |
| probe-1-before | 5.637 | 545 | 702 | 213 | 217 | 104 | 464 | 776 |
| probe-1-after | 4.767 | 460 | 597 | 201 | 223 | 80 | 332 | 581 |
| probe-2-before | 5.951 | 636 | 707 | 234 | 227 | 100 | 499 | 625 |
| probe-2-after | 5.282 | 484 | 638 | 235 | 240 | 98 | 255 | 759 |
| probe-3-before | 6.679 | 634 | 721 | 269 | 245 | 117 | 427 | 858 |
| probe-3-after | 6.143 | 577 | 631 | 220 | 690 | 161 | 720 | 629 |

最终无 probes 的进程墙钟 after/before：**0.778 / 0.545 / 0.850**。探针系列：**0.846 / 0.888 / 0.920**。
不要把其中一次异常慢的 objects 或 regex 当成稳定速度差：负载、并发 GC、编译时序有明显波动。
早期探索系列也保存在 JSON（其 probes 的转移归属与最终版不同，不能混算成本占比）：
无 probes 墙钟 9.712→12.533、7.200→5.060、5.166→4.009 s，首轮确实回退，未删除。
因此最可靠结论是消除了对齐重做，以及事件计时下降；**不宣称已证明通用/稳态 JS 加速比**。

### 每次运行前的 uptime（最终系列）

```text
plain-1-before      22:17  2 users, load averages: 6.73 10.36 11.18
plain-1-after       22:17  2 users, load averages: 6.51 10.25 11.14
plain-2-before      22:17  2 users, load averages: 6.39 10.16 11.10
plain-2-after       22:17  2 users, load averages: 7.00 10.23 11.12
plain-3-before      22:17  2 users, load averages: 6.92 10.16 11.09
plain-3-after       22:17  2 users, load averages: 6.48 9.95 11.01
probe-1-before      22:17  2 users, load averages: 6.48 9.95 11.01
probe-1-after       22:18  2 users, load averages: 6.34 9.81 10.94
probe-2-before      22:18  2 users, load averages: 6.07 9.69 10.89
probe-2-after       22:18  2 users, load averages: 5.98 9.62 10.86
probe-3-before      22:18  2 users, load averages: 6.06 9.57 10.84
probe-3-after       22:18  2 users, load averages: 6.30 9.56 10.82
```

完整、未舍入的最终/探索结果、states、metrics、flags 和 uptime：
[`C-JITCODE-measurements.json`](C-JITCODE-measurements.json)。
原始 stdout/stderr、每进程 profile 与汇总：`target/phase2-c/ab-v55/<label>/`；探索记录在 `target/phase2-c/ab-final/`。

## 5. 检查结果与复现命令

最终代码执行：

```sh
python3 tests/attesor/jitcode-sensitivity.py
python3 tests/attesor/jitcode-measure.py --output target/phase2-c/another-fresh-run
python3 tests/attesor/jitcode-check.py
python3 tests/attesor/jitcode-tls-check.py
python3 tests/attesor/profile-report-check.py
ATTESOR_JIT_ON=jitcost,slotreuse python3 tests/attesor/profile-check.py --output target/phase2-c/another-profile-run
```

结果：
- `cargo test --release --workspace -- --test-threads=1`：四种配置（无开关 / slotreuse / jitcost / 两者）均 **66 + 9 passed，0 failed**。
- COMMON 全部 13 类 `blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops calls fpnan tbiblocks`：各 2000 次，四种配置均 `mismatch=0`。
  `loops` 额外分别运行 seeds `1 / 0x1234 / 0xdeadbeef`，每个 seed 2000 次。
- 新 `jitcode`：四种配置、三个 seeds，每次 512，全部 `mismatch=0`；slotreuse 全八种 host 地址低三位均被覆盖。
- `tests/attesor/loopra-aot-check.sh`：四种配置全部 **7/7 native=interp=cold-JIT=warm-AOT**，cache identity 检查通过。
- 额外 `callret,jitcost` / `callret,slotreuse` / 三者组合：`calls 2000` 与三 seed `jitcode 512` 全部 `mismatch=0`。
- 敏感性证明（故意修改真实生成路径，随后自动还原并重建）：
  - 删除 continuation 的 prefix 修正：`jitcode 256 0x1234`，**243 个 MISMATCH**（标签审计/差分失败）。
  - 把间接计数器 INC 错写到 guest x0：同一测试 **256 个 MISMATCH**。
  - 还原后三个 seeds 各 `512/512`，`mismatch=0`。不是只以进程崩溃充当敏感性证明。
- profiler merger、老格式兼容、新指标 round-trip/负数拒绝通过；真实 guest 的 JIT/interp、线程、fork、成功/失败 exec、10 秒 active-call snapshot 和受控 SIGKILL 检查通过。
- 原样运行 `docs/tasks/phase2/t.js`，四种配置的 SHA256/SHA512/AES-GCM/ECDSA/ECDH/Unicode 校验通过。
  **公网 TLS 未验证：全部为 `https error ENOTFOUND`，没有得到要求的 `https status 401`。** 沙箱还拒绝本地 TCP bind，不能用本地服务冒充公网检查通过。

日志：`target/phase2-c/checks-final.log`、`checks/`、`sensitivity-final.log`、`sensitivity/`、`callret-final/`、`tls-final.log`、`tls-final/`、`profile-check-final.log`。
编译仍有仓库既有 warnings（包括 SDK/confstr 与 extern 声明）；没有把它们包装成零 warning。

## 6. 未决风险 / 接受条件

- 默认关闭，尚未经过 Claude 接受；不自动启用 `jitcost` 或 `slotreuse`。
- 整体移动使 host entry 不再保证 16-byte 对齐，可能影响某些稳态热路径；需在更长的 V8/JSC/Wasm/.NET/regex 工作负载上独立 A/B。
- 复用依赖 `Unit` 的所有相对代码/数据引用均在单元内这一现有约定；将来若新增外部 PC-relative 引用，必须更新或禁用此路径。`reloc_infos` 检查不能代替该约定。
- 当前只测了此机器、此 Electron/Node helper；没有证明跨引擎、跨 CPU 或 GUI 应用收益。
- 公网 TLS 的 401 是缺失验证项，联网环境必须补跑；密码学校验通过不等于 TLS 全链路通过。
- probes 有开销；匿名 PC 分类、1 ms 采样、线程并行及 whole-process 启动成本限制结论精度。低样本分支行尤其不能作细小差异推断。
- syscall/shared-region 映射和实际 guest 执行仍是大成本；本次没有在缺少进一步 operand/指令热点证据时盲改它们。
- 失效覆盖范围仍保守；新指标只能说明该基准下重翻译不大，不等于其它 SMC 模式无需改进。

## 7. Git / 提交交接

未提交、未 push：`git add` 返回 128，Git worktree 的 `index.lock` 创建被拒绝（Operation not permitted）。
详情见 `target/phase2-c/git-write.log`。工作保留在本树，交付可顺序应用的两个逻辑步骤：

1. 提交说明：`target/c-01-commit-message.txt`；补丁：`target/c-01-profile.patch`。
2. 提交说明：`target/c-02-commit-message.txt`；补丁：`target/c-02-slotreuse.patch`。

两个补丁已在私有临时目录顺序通过 `git apply --check` 和实际应用，并逐字节重建全部最终修改/新增文件；日志 `target/phase2-c/patch-check.log`。

所有应用运行均为 headless；未启动 GUI、修改用户应用或共享 Attesor cache。所有自建 cache 在本工作树 `target/` 或私有临时目录。
