# #29：父 Attesor 自 fork、子进程原地装载新 guest 的可行性

2026-10-29，Track P Round 2，基点 `70c4f28`。这是**可行性审查，不是实现或性能验收**。
按照任务先完成 #30 的实现与逐页对照，再审查本方案；没有创建 warm server、修改 spawn
路径或假装测到了 no-exec 加速。#28 不在本轮工作范围。

## 结论：当前代码不具备安全实现前提，本轮不实现

新设计确实去除了 Round 1 所述的“server 的孩子不属于 guest parent”问题：让实际父
Attesor 调用 libc fork，孩子仍是真实内核 child，PID/PPID、waitpid、SIGCHLD 不需要
代理。**这只解决亲缘关系，不自动提供 exec / posix_spawn 的状态转换。**

阻碍不是“Rust 中不能调用 fork”，而是要在任意多线程 guest 的 fork 子进程中，不 exec，
继续分配内存、获取锁、重建 loader、执行 dyld，同时严格丢弃旧 guest 的所有状态。
现有代码没有覆盖这些状态的协议；按本任务只在风险可解时实现的要求，决定为 NO-GO。
这不表示理论上不可能；以下列出需要先交付、再验证的依赖。

## 当前实现证据及所需协议

| 范围 | 已读代码 / 事实 | no-exec 所需工作与判断 |
|---|---|---|
| fork 同步边界 | `aot.rs::fork_safely` 依次保护 profile、CPU registry、pending batches、AOT images/seen/file locks；`fork_guest` 重启 checkpoint/profile | 不包含 JIT 的 `TRANSLATE/BLOCKS/INDEX`、symbols、cache、signals、workq、FD path map 等锁。不能在 child 盲目重置锁，可能继承半写的数据结构。需要全局 lock-order、所有 runtime worker 的 quiescence 与 child 重置协议；目前没有 |
| host runtime / allocator | 当前 `syscalls.rs` fork 路径调用 libc fork，而非 raw fork，目的是让 host libSystem 修复 task/reply-port 等状态 | 这是必要条件，不是对 Rust 全局锁、guest malloc/ObjC/libdispatch、native/GPU 库状态的充分证明。`LIVE_THREADS==1` 只数 guest，不涵盖 sampler/checkpoint/workq monitor/host library threads。不能把一次 fork 成功当作多线程安全证明 |
| 主映像 / dyld / stack | `loader.rs::Image/Loaded` 没有释放映像的 Drop；`map_image` 先非 MAP_FIXED 预留固定 MAIN_BASE/DYLD_BASE，有旧映像就失败；stack 单独映射 | drop Loaded 或新建 Cpu 不会卸载旧映像。需要明确的 GuestAddressSpace 所有权、可回滚的新装载过程以及 child 自己的 host stack；必须先复制所有 argv/env/spawn blob，不能先解除它们所在的 guest 映射 |
| guest VM 的完整性 | `syscalls.rs::mmap_guest` 管部分 mmap；Mach VM allocate 有 64 GB deterministic arena；其他 Mach map/remap/allocate 和 munmap/protect 仍会转发。profile mmap 记录不是 VM 所有权账本 | 必须覆盖所有 guest VM 操作、alias、MAP_FIXED 替换和拆分。枚举 host VM 后按地址范围批量 munmap 不能区分 guest 堆、host allocator、host stacks、JIT code、commpage/bridges。不允许靠猜地址区间清场 |
| 共享缓存并非全只读 | `cache.rs` 保存 ranges/slide，映射数据为私有可写页；guest dyld、libSystem、ObjC、malloc 等会修改进程局部数据 | 可复用 immutable text / 原始预滑动数据来源，不能直接复用 parent 运行后的 mutable cache data：其中含旧 guest heap/TLS/ports/锁/初始化标志。须还原到 guest 运行前的干净数据视图，并重新执行针对新进程的初始化。磁盘 slid cache 只是此干净来源之一，不是进程快照 |
| threads / TLS / workqueues | `threads.rs` 的 REGISTERED、start PCs、TSD offsets、LIVE_THREADS；workq 的 IDLE、MANAGER、WL、monitor kqueues、Once 和 TLS | 原子值可重设不代表所指 guest memory/ports 可保留。继承 worker records 没有对应线程；旧 REGISTERED 会令新 dyld 注册失败。需要 reset + 资源归属，不能“清空 Vec”代替销毁协议 |
| 已加载 AOT/JIT | `aot.rs` 的 image.loaded、profiles、LAST_IMG 与地址相关；JIT 的 blocks、units、incoming chains、IBTC/HOT/RAS/epoch 与旧进程代码相关 | 只能保留确证与新映像/slide/translator build 相容的 immutable translations。当前没有公开的“换整个 guest generation、撤销所有旧链并保留缓存代码”API；AOT 外部 block 的失效限制也不能绕过。需要 core track 提供 API，与 #28 的 wave 2 边界有关 |
| 信号 | `signals.rs` 同时保留 guest ACTIONS 和 host ignore/default dispositions；异步 guest handler delivery 本身尚未完整实现 | exec 会重置信号处理、处理 alt stack/CLOEXEC 等，fork 不会。旧 handler PC 可能随后被 unmap。需要 child 的新进程语义转换，保留 mask/ignore 的规则及 spawn 属性优先级，不能承诺修复当前所有信号限制 |
| FDs / actions / attrs | `spawn.rs::exec_like` 重写路径/argv/env，保留原始 syscall 244 参数由内核执行；没有解码完整 Darwin spawn actions/attrs | fork 只提供继承的 FD 表，不关闭 CLOEXEC，也不应用 open/close/dup2/chdir、CLOEXEC_DEFAULT、sigdefault/mask、pgroup、resetids、start-suspended、SETEXEC、ports 等。需要版本化且有界的 Darwin descriptor 解码与精确定义；未知 action/attr 必须在 fork **之前**回退现有 exec 路径 |
| Mach / native bridges | `syscalls.rs` 直接转发大量 Mach 调用，`iokit.rs` 有 fake-port map，GPU/native bridge 有 host 状态与 TLS | libc atfork 只照料 host runtime；guest 记录的 Mach names、reply ports、pthread identity、IOKit/bridge sessions 不保证对应 child task。必须逐类重建/拒绝，不宜复制继承后假定可用 |
| 失败与 parent 行为 | 现有 spawn 失败由 host syscall 同步返回 errno | 新路径须有私有错误通道、父进程等待装载成功协议，失败 child 必须 reap，不能把失败变成“成功生成一个退出 127 的孩子”。阻塞期间还需处理信号与 parent exit；实际错误语义须与当前路径逐项对照 |

Apple 的 libc fork 源码确实调用 libSystem prepare/parent/child handlers；源码没有给
Attesor 任意 guest/host 库的重置保证。[Apple Libc fork](https://github.com/apple-oss-distributions/Libc/blob/main/sys/fork.c)

多线程 fork 后仅存调用线程，可能继承其他线程持有的锁；在 exec 前调用非
async-signal-safe 功能的限制不能因使用 Rust 或 libc fork 自动取消。
[QNX 官方 POSIX fork 说明](https://get.qnx.com/developers/docs/7.0.0/com.qnx.doc.neutrino.getting_started/topic/s1_procs_Multithreaded_fork.html)

FD、signal mask/default、身份与进程组的 spawn 语义可见
[Apple posix_spawn 手册](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/posix_spawn.2.html)；
Darwin 的 opaque attributes 还有 port actions 等扩展，不能只实现标准的几个 flag。
[Apple XNU spawn_internal.h](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/spawn_internal.h)
这些公开资料是约束依据，不是对本机内核所有私有 ABI 已完成验证的宣称。

## 建议的实现顺序与验收门槛（尚未执行）

1. **所有权和重置先行**：GuestAddressSpace、线程/FD/Mach resource ledger；干净 shared-cache
   writable data 的恢复；不碰 host runtime mappings。用反复 load/unload 的非 fork CLI
   验证第二次 bsdthread_register、init、TLS、signals 与 mapping census。
2. **core API**：显式 guest generation switch，保留经验证的 immutable cache/AOT 单元；
   废弃其他 PC、incoming slots 和 TLS caches。不能以 `register_block` 代替生命周期登记。
3. **fork contract**：先做严格单线程、没有 profiler/checkpoint/workq/native/GPU 初始化的
   opt-in 子集；需要可证明的 host 单线程检查，而不是 LIVE_THREADS。该子集无法证明满足
   Electron 的多线程父进程，也不保证有显著预热收益。完整方案需所有协作线程停在安全点，
   所有 host 库的 fork 可用性有依据，失败时在 fork 前走已有 exec。
4. **spawn contract**：完整支持的 action/attr 白名单，先深拷贝与校验，再 fork；错误通道，
   async-safe child bootstrap；unsupported、SETEXEC、身份/port 等路径优先原样回退。
   路径、argv[0]、脚本解释器、env 必须沿用现有约定。
5. **先正确性再性能**：至少 1,000 次多线程 spawn 压测；并发 JIT/AOT/profile/workq；child
   FD 图、CLOEXEC/dup2/open/close/chdir、IPC、waitpid/WNOHANG/SIGCHLD、signal mask/default、
   pgroup/stop/resume、错误 errno、parent exit；不泄漏映射/ports/FDs，不遗留 worker。再进行
   trivial arm64、headless Node `-e 0`、6 children 的 >=3 组交替 A/B，区分 parent spawn
   latency、child-ready、launch-to-exit、aggregate CPU、COW RSS 和 translation CPU。

这里没有执行上述 no-exec 压测，也没有发布一个只对简单父进程“偶尔能跑”的开关。
本轮 #30 的 trivial / Node 数据仍是普通 exec 的 launch-to-exit 控制组；不是 no-exec
spawn latency。Round 1 冷启动数据也不能当作可节省时间的下界/保证。

**决策：维持当前 exec / posix_spawn 路径；#29 暂缓到所有权、generation reset 和 fork
协议具备可验证实现之后。** 未发现能够在本轮 process-only 边界内解决全部风险的捷径。
