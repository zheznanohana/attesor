# Track P：进程层测量与实验报告

日期：2026-10-29（JST）。工作树 `/Users/zhez/attesor-p2p`，分支 `process`，
基点 `35cbfb195bac5dea68e12406ac45cdc58751b714`。

**状态：部分完成，不能将整条 P 标记为完成。** 先测量，再实现安全范围内的实验。
未修改 `jit/emit.rs`、`jit/mod.rs` 或其他 translator core；没有启动 GUI、修改用户应用、
使用用户 Attesor 缓存、推送或操作非本次任务启动的进程。

## 1. 实际改动

- `tools/process-bench.py`：至少三组 OFF/ON 交替运行；私有冷缓存、日志、逐进程 profile、
  原始 wall/CPU 秒数和 `uptime`。覆盖 trivial、headless Node、6 个 Node children、
  offline prewarm、Node 与 prewarm 竞争、JS、可选 TLS。超时只清理自己创建的进程组。
- `tests/attesor/process-fork.js`：实际检查 6 个子进程的 argv、环境、IPC、PID 和退出状态。
- `tools/process-container-bench.py`：签名的私有 arm64 CLI guest，私有 HOME，交替关闭/启用
  container mapping。验证目录确实只在启用组生成；不应用 host sandbox，也不启动应用。
- `process_qos.rs`、`aot.rs`、`main.rs`：**仅实验性**的
  `ATTESOR_JIT_ON=process-qos`，`ATTESOR_JIT_OFF=process-qos` 优先。
  只处理不再执行 guest 的 detached AOT exit/exec-save builder，以及不运行 guest 的
  `--prewarm` 调用。同步 AOT save、失败 exec 返回路径、guest/UI 线程均不降级。
  现有 checkpoint worker 本来已经是 background QoS，没有改动。
- 实测 guest 执行后的 fork builder 调用 pthread QoS API 返回 `EPERM`；增加
  `PRIO_DARWIN_THREAD/PRIO_DARWIN_BG` 回退，并读取实际线程后台状态。
  不使用进程级 nice/task policy。诊断用 raw write，避免 fork 子进程获取 Rust stderr 锁。
- `tests/attesor/process-qos-check.py`：真实 prewarm、同步 save/失败 exec、异步 builder
  检查；等待自己的异步 builder 退出，不留下后台构建进程。

没有改变生成代码，因此 `TRANSLATOR_VERSION=53` 保持不变。集成测试逐字节验证 OFF、ON、
OFF 优先三种配置的 AOT block 数、索引和代码 payload 相同；兼容性 ID 本来就包含完整
JIT 开关字符串，仍保留这种隔离。

**QoS 的正确性检查通过，但性能验收失败。开关保持默认关闭，不推荐启用或合并为默认行为。**

## 2. 测量方法和原始结果

先构建无改动 binary，保存为 `target/p-process/attesor-before`，完成三轮基线后才改 runtime。
所有缓存均在 `target/p-process/` 或测试自己的 `/private/tmp` 临时目录。

- `CPU s` 来自 `RUSAGE_CHILDREN`：本次运行所有被等待子进程的累计 CPU 时间，**不是翻译 CPU 时间**。
- profile 的 `translate_elapsed_ms` 是 JIT 翻译经过时间，包含争用，**不能冒充 CPU 秒数**。
- 启动时间为命令启动到退出，包括 dyld/缓存工作和退出，不是仅 host `posix_spawn` syscall。
- OFF/ON 使用同一最终逻辑实现的 release binary，独立冷缓存，按 OFF、ON 顺序交替三次。
  普通 guest 测量禁用 exit/checkpoint AOT build，避免脱离计时的后台工作。
- contention 同时启动一个独立 offline prewarm 和 `node -e 0`，等待两者结束；
  foreground 时间、总等待时间、累计 CPU 都保存。后台完成时间仅为等待后的观察上界。

### 无改动基线

| 工作负载 | wall s（三轮） | CPU s（三轮） |
|---|---|---|
| trivial arm64 | 0.889000, 1.209342, 1.385886 | 0.849829, 0.835511, 0.892619 |
| headless Node `-e 0` | 2.483415, 2.236867, 2.560608 | 2.038859, 2.068426, 2.161250 |
| Node 父进程 + 6 children | 4.668122, 12.120348, 4.697855 | 17.506482, 26.022003, 17.474071 |

7 个进程的翻译 block 总数：259265、259533、259619；
翻译经过时间之和：10996、21457、10775 ms。第二轮明显受外部负载影响，未删掉这个样本。

逐进程 block 数 / 翻译经过 ms（每行父进程后接六个 child，PID 见原始 profile）：

| 轮次 | 父进程 | child 1 | child 2 | child 3 | child 4 | child 5 | child 6 |
|---|---|---|---|---|---|---|---|
| 0 | 46860/1541 | 35407/1559 | 35529/1604 | 35385/1572 | 35432/1583 | 35338/1587 | 35314/1550 |
| 1 | 46730/2584 | 35412/2949 | 35399/3172 | 35542/3206 | 35420/3265 | 35467/3126 | 35563/3155 |
| 2 | 46790/1528 | 35484/1579 | 35342/1484 | 35396/1541 | 35554/1562 | 35443/1536 | 35610/1545 |

`uptime` 基线样例：`22:07 2 users, load averages: 11.33 10.93 10.22`；
异常轮：`22:07 2 users, load averages: 12.21 11.12 10.30`。
完整原始记录：`target/p-process/baseline.jsonl`、`baseline-*/profile/*.txt`。

### QoS OFF/ON 三组交替结果

每个单元格保留三轮原始数；ratio 为 ON 均值 / OFF 均值，不是选最好一轮。

| 工作负载 | OFF wall s | ON wall s | wall ratio |
|---|---|---|---|
| trivial | 0.917167, 1.133582, 1.281719 | 0.873761, 0.815218, 1.231610 | 0.876 |
| Node `-e 0` | 2.234337, 1.977975, 2.443982 | 2.150784, 2.348722, 2.056015 | 0.985 |
| 7 processes | 5.703918, 4.465643, 4.780636 | 4.749663, 4.315181, 4.544109 | 0.910 |
| prewarm | 0.286264, 0.228695, 0.287587 | 1.121730, 0.964095, 0.966718 | **3.804** |
| contention | 2.331910, 1.876077, 2.249561 | 5.250228, 1.914701, 2.407407 | **1.482** |
| JS | 4.775683, 5.161112, 4.419346 | 4.442575, 4.674549, 4.531755 | 0.951 |

| 工作负载 | OFF CPU s | ON CPU s | CPU ratio |
|---|---|---|---|
| trivial | 0.879509, 0.801182, 0.744522 | 0.869950, 0.791798, 0.817225 | 1.022 |
| Node `-e 0` | 2.313427, 2.093134, 2.114574 | 2.219201, 2.069133, 2.104734 | 0.980 |
| 7 processes | 18.362392, 14.180429, 13.473428 | 17.087688, 13.699889, 15.849428 | 1.013 |
| prewarm | 0.246445, 0.201340, 0.234242 | 1.024769, 0.926538, 0.923499 | **4.215** |
| contention | 2.406523, 2.181349, 2.218149 | 3.121227, 2.908904, 2.895412 | **1.311** |
| JS | 5.786475, 5.854917, 5.800496 | 5.822230, 6.176473, 6.105486 | 1.038 |

prewarm 每次都是 2 images / 30412 blocks，且 AOT payload 相同。
不能因普通 guest 的 noisy wall ratio 小于 1 就声称 QoS 提速：这些普通运行不启动 AOT builder。
独立预热和竞争场景不满足“没有变慢”，所以 **#35 性能未验收**；具体原因（CPU 频率、
调度、I/O 等）未进一步隔离，不能编造归因。

7-process A/B 翻译总 block 数 OFF：261212、260978、261057；ON：260733、261151、261185。
这不是共享缓存实验，也没有证明减少重复翻译。

A/B `uptime` 开始：`22:21 2 users, load averages: 10.15 10.66 11.10`；
结束：`22:22 2 users, load averages: 6.22 9.35 10.57`。
JS 开始/结束 load averages：`5.97 9.25 10.52` / `5.23 8.81 10.33`。
逐次 `uptime`、全部逐进程计数、日志在：
`target/p-process/ab/measurements.jsonl`、`js-ab/measurements.jsonl`，汇总 `summary.txt`。

## 3. 各方向结论及未完成项

### #28 跨进程共享 live translations：未实现，有明确 API 阻碍

重复翻译已由 7-process 基线验证。但当前公开 `jit::register_block` 只向 `BLOCKS` 插入地址，
不登记 `UnitRec`、guest 范围、pages、incoming slots。`jit::invalidate_range` 只遍历这些
本地 index 项；INDEX 不存在时直接返回。见 `jit/mod.rs:187-190,361-365`。
因此用它装载新的 live shared Plain blobs，不能保证要求的逐进程失效、撤链及 epoch 安全。
现有 AOT 的这项限制不能当作新功能的安全许可。

另外，现有 AOT loader 特意复制到匿名 code cache：直接从磁盘 `PROT_EXEC` 映射可能触发
Gatekeeper GUI。没有为了做实验而尝试这种映射。共享内存方案仍需要安全的外部 unit
登记/失效和生命周期 API。按照禁止修改 translator core 的边界，本轮不绕过它、也不发布
只能通过随机循环而无法安全失效的实现。需要与 core invalidation track 协作后才能完成。

### #29 zygote：未实现

已经测到 trivial 和 Node 的真实 launch-to-exit 基线，但没有实现或测到 zygote 加速。
已有 `aot::fork_safely` 只保护 profile/CPU registry/AOT 锁；文档也明确不保护 emulator 的所有锁。
运行过 dyld/Node 的 warm template 不能据此被认定可安全 fork。

此外，server fork 出的孩子属于 server，不属于原 guest parent；直接把 PID 回传并不能维持
`waitpid`/SIGCHLD、spawn fd actions、信号、环境/argv 和 Mach task/reply ports 的语义。
没有实现这些虚拟化/重置协议，不能把普通 guest fork 或现有磁盘 AOT 说成 zygote。

### #30 dyld closures：调查后本轮不可安全实现

Node 基线 profile 中，`JustInTimeLoader::applyFixups` block invoke 为 78 / 328 个
JIT+interp execution samples（23.78%），另有 exported-symbol lookup、resolveSymbol、
map-with-linking 等。这个比例只针对执行样本，不是整个启动 CPU 比例。

已有 pre-slid DSC 数据缓存并不是 launch closure：Node 首轮实测 lookup 0.040 ms、
mmap 29.131 ms、slide 499.440 ms、publish 659.089 ms；后续 child 可命中这个已有缓存。
它只保存已重定位的缓存数据页。dyld RuntimeState/JustInTimeLoader、library slides、
依赖/符号绑定、环境、load order 和可变数据没有可用的稳定序列化/恢复接口。
直接持久化 guest 指针或跳过 dyld 步骤不能保持这些语义；本轮没有提供未经验证的 closure 快照。
证据：`target/p-process/baseline-node-report.txt` 和原始 stderr/profile。

### #23/#24 Mach IPC、同步、路径与 container mapping

三轮 7-process profile 的仿真 elapsed wall overhead（扣除已显式计时的 host 调用）：

| 类别 | 三轮 ms | 与同轮 aggregate CPU s 相比较的比例范围 |
|---|---|---|
| Mach IPC (`mach_msg*`) | 0.320708, 0.471107, 0.311038 | 0.00178%–0.00184% |
| psynch/ulock | 0.657429, 1.023428, 0.643551 | 0.00368%–0.00394% |
| path-taking syscalls | 3.670142, 12.451303, 3.933601 | 0.02096%–0.04785% |

这是 elapsed overhead 与累计 CPU 的保守量级比较，**不是直接测得的 CPU 百分比**；
profile 只覆盖其已标注的 host 边界。大量 kernel wait/idle 不能算成仿真开销。
第一轮 83.99% 的全部线程样本为 idle，也不意味着 IPC emulation 占用了 84% CPU。
未发现这几个仿真类别达到 5% 的证据，因此没有为它们添加缓存或更改语义。

真实签名的 private sandbox CLI，现有 pre-slid cache 已 warm，关闭/启用 container mapping：

| 指标 | OFF（三轮） | ON（三轮） | 均值 ratio |
|---|---|---|---|
| wall s | 0.128409, 0.120092, 0.124182 | 0.124709, 0.127482, 0.129033 | 1.023 |
| CPU s | 0.140484, 0.131524, 0.129128 | 0.132311, 0.136696, 0.138013 | 1.015 |

转为正式工具后又复现三组：OFF wall 0.177242/0.190856/0.183209，ON
0.168533/0.190340/0.208054；CPU OFF 0.181317/0.202094/0.190963，ON
0.169725/0.197007/0.219910。两批数据均有噪声，整体没有稳定超过 5%；单个样本不能作结论。
实际 container 目录验证通过。第一次 `uptime` load averages `6.22 9.35 10.57`，
复现 `2.92 5.50 8.48`。原始数据在 `target/p-process/container*/measurements.jsonl`。
这仅覆盖 CLI 容器路径设置，不证明大型 sandbox GUI 应用的一切开销。

## 4. 测试行与敏感性证明

开关两组均运行：

```text
cargo test --release --workspace -- --test-threads=1
  attesor: 68 passed; core: 9 passed; 0 failed（OFF、ON 各一次）
--selftest-jit {blocks,fuse,fuse2,selchain,fpblocks,dpimm,dpreg,ldst,branch,loops,calls,fpnan,tbiblocks} 10000
  所有类别 mismatch=0（OFF、ON）
--selftest-jit loops 3000 {1,0xdeadbeef,0x100f5e1f7e57cafe}
  三种 seed 均 mismatch=0（OFF、ON）
tests/attesor/loopra-aot-check.sh
  7 workloads: native=interp=cold-JIT=warm-AOT; cache identities match（OFF、ON）
process_qos unit tests: 2 passed
process-qos-check.py:
  PASS prewarm: explicit ON, OFF precedence, actual QoS=0x9, identical AOT indexes/code
  PASS native=interpreter=JIT/sync AOT output; failed exec returns; sync guest QoS untouched
  PASS detached exit builder: qos_result=1 qos=0x21 darwin_result=0 darwin_bg=1; published AOT and exited
JS: 7 个结果在 3 组 OFF/ON 中完全一致
crypto/Unicode: OFF=ON；ECDSA verify=true；ECDH=true
TLS: https error ENOTFOUND（两组）；未验证 https status 401
```

现有 loopra AOT 脚本会清除继承的 `ATTESOR_*`。ON 组使用
`target/p-process/attesor-qos` wrapper，确保每个 translated guest 子调用都显式设置 flag，
而不是只在外层 shell 设一个会被脚本清掉的变量。wrapper 和执行列表在
`target/p-process/regression.sh`。日志在 workspace-{off,on}.log、selftest-{off,on}.log、
loopra-aot-{off,on}.log、qos-integration.log、js-values.log、tls-check.log。

两次故意破坏实现并恢复：

1. 将 background QoS 从 `0x09` 改成 `0x11`，真实 worker 状态比较出现 `(0,17)` 与
   `(0,9)` 不匹配，测试 exit=101；`qos-sensitivity.log`。
2. 将 Darwin fallback 设置改为清除后台状态，读回 `(0,0)`，预期 `(0,1)`，
   测试 exit=101；`qos-fallback-sensitivity.log`。

恢复后重新执行完整开关矩阵。没有改生成代码，故不存在新增 operand form。
另保存第一次集成测试发现 EPERM 的失败记录 `qos-integration-initial-failure.log`，未掩盖为通过。

## 5. 重现、风险、提交材料

```sh
TMPDIR=/private/tmp cargo build --release
python3 tools/process-bench.py --output target/p-repro --workloads trivial node fork prewarm contention js
python3 tools/process-container-bench.py --output target/p-container-repro --sysroot /path/to/arm64-sysroot
python3 tests/attesor/process-qos-check.py
python3 tools/profile-report.py target/p-repro/0-fork-off/profile
```

主要风险/剩余工作：

- **#28/#29 未实现；#30 没有安全 closure 实现。** 不能宣称已完成共享 live cache、zygote 或
  dyld launch persistence。逐进程翻译 CPU time 没有单独 instrumentation；本报告区分了
  aggregate process CPU 和逐进程翻译经过时间。
- **#35 不满足性能验收**，尤其不能默认降低用户主动等待的 prewarm 优先级。当前候选仅为
  关闭状态的可复现实验，需 Claude 审核决定修改/丢弃，不是已接受优化。
- DNS 不可用，TLS 的 401 检查仍待网络可用环境；没有把 crypto 成功等同 TLS 成功。
- host 负载变化明显；这里没有 GUI 启动、真实 UI 响应或完整 Electron 多角色进程的性能结论。
- 已有 AOT 外部登记的失效限制，以及 existing fork 对其他全局锁的限制未修复。
- 启用 QoS flag 仍使现有 AOT codegen identity 分离，即使代码 payload 相同；不放宽校验。

Git worktree 的 metadata 指向禁止写入的主仓库路径，不尝试绕过权限或 push。
一个逻辑步骤保留在当前工作树，并提供：

- 提交说明：`target/p-commit-message.txt`
- 完整补丁（包括新文件）：`target/p-process.patch`
- 测量与检查产物：`target/p-process/`（包括原始数据、profile、日志、私有缓存）

