# DSCSLIDE（stage 2c）实施报告

工作树：`/Users/USER/attesor-dsc`；分支：`dscslide`。未提交、未推送。
实现仅修改本工作树；所有运行使用独立 `ATTESOR_CACHE`，没有使用用户的 Attesor 缓存，
没有启动用户应用、终止无关进程或清空系统页缓存。

## 1. 测量

需滑动的映射（`slide_size != 0`）共 **493,780,992 字节（470.906 MiB）**。
固定基址 `0x600000000`，本机实际 slide 为 `0x480000000`。

下表为最终二进制各 5 次诊断运行的中位数，交替启用/禁用；没有页样本输出。
这是单独的分段测量，不混入下节无诊断的启动墙钟计时。

| 路径 | 查找及完整校验 | mmap pass（含覆盖映射） | apply_slide_info（含最终 mprotect） | 滑动写脏的主机页 |
|---|---:|---:|---:|---:|
| 禁用缓存 | 0.000 ms | 0.249 ms | 451.549 ms | 88,209 |
| 热缓存 | 193.692 ms | 7.620 ms | 0.000 ms | 0 |

主机页大小 4,096 字节；计数是滑动存储触及的唯一主机页（包括跨页的非对齐指针），
不是客体开始运行后整个进程的所有脏页。原始数据和逐次日志：
`target/dscslide-verified/diagnostics.json`、`diagnostic-{disabled,warm}-*.err`。
最初基线探针还测到 mmap 34.103 ms、slide 512.653 ms、23,263 个含指针的 16 KiB dyld 页，
见 `target/dscslide/baseline.err`；不要把 dyld 页数与上述 4 KiB 主机页数混用。

## 2. 设计与磁盘预算

- `cache/slid.rs`：格式 v1；身份包含主缓存及每个 subcache 的 UUID、slide、主机页大小、
  文件与映射的归属，以及完整映射表的 address/size/file_offset/slide_size/slide_start/max_prot/init_prot。
  不使用易变的 fd 数值作为身份。命名用摘要，读取时还逐字节比对完整身份，避免仅依赖文件名摘要。
- 记录滑动真正写入的主机页，合并相邻页为 extent。先保留原始文件映射，再用缓存的 extent
  以 `MAP_PRIVATE | MAP_FIXED` 覆盖；保护仍为 `init_prot & 3`，绝不增加 host EXEC 权限。
  未写入页仍使用原文件，不复制整个数据映射。
- 文件含 56 字节固定头、完整 key、extent 表、对齐 padding 和页数据。
  使用前验证 magic/version、精确身份、精确长度、extent 顺序/边界/页对齐、总 payload 长度，
  并校验所有头部元数据、padding 与 payload 的 64 位字级非加密校验和。
  这是损坏检测，不是安全认证；目录信任边界与现有 AOT 缓存相同。
- 非阻塞独占 flock；miss 完成正常滑动后，在锁内再次检查，只有一位写者发布。
  使用唯一临时文件、`sync_all`、原子 rename；任何错误回退原有路径。
  若覆盖映射中途失败，先重新映射**全部**原始区域再滑动，避免二次处理已滑动指针。
- 只在发布阶段持有 AOT 的 `FILE_LOCKS` 读侧，避免 fork 继承写锁 fd；
  不跨 `profile::host_call` 持锁，遵守 profiler → runtime → AOT 的锁顺序。
- AOT `tidy_cache` 清理旧格式、已死写者的临时文件、30 天未使用的数据；
  使用时对打开的 inode `futimes`，过期判断使用 mtime。清理在同一发布锁内复核，
  不误删刚完成的发布。零长度锁文件保留稳定 inode，避免 unlink 后出现两把锁。
- 与代码生成版本独立；`--no-aot` 也可命中。诊断/禁用/字节采样开关记录于 `docs/DEBUGGING.md`。

**缓存文件 361,369,600 字节（344.629 MiB，约 361.4 MB）**。
其中页数据 361,304,064 字节，头/索引/对齐 65,536 字节，共 2,281 个合并 extent。
由于整段数据约 493.8 MB 超过约 400 MB 预算，已经采用只存被滑动写入页的方案，
实际文件低于预算；不是以“大文件可接受”为理由保留全量副本。

## 3. 每进程启动墙钟时间

相同 CoreFoundation arm64 程序、`--no-aot`、私有缓存、无 profiler/trace/页样本/统计日志。
每种状态 **5 次**；交错并轮换四种状态的执行顺序，保留全部样本，不选择最快值。
original 是实施缓存前保留的基线二进制（仅增加分段计时及 dyld 滑动页计数），
disabled 是最终实现设置 `ATTESOR_DSC_DISABLE=1`；cold 每次删除此测试自己的预滑动文件后重建；
warm 每次确认已有有效文件。没有 flush OS 文件缓存，也没有运行自己的 build/selftest 干扰计时。

| 状态 | 5 次墙钟时间（ms） | 中位数（ms） |
|---|---|---:|
| original | 1089.023, 707.499, 801.316, 739.066, 713.411 | 739.066 |
| disabled | 1003.449, 773.126, 748.014, 776.720, 752.679 | 773.126 |
| warm | 699.728, 447.922, 447.501, 433.368, 411.760 | 447.501 |
| cold | 1298.343, 1016.941, 951.153, 1152.817, 1076.781 | 1076.781 |

original → warm 中位数减少 **39.5%**。首次创建文件有额外校验和、写入及同步成本，
不声称 cold 更快，也不把这个小程序结果外推为完整应用的启动加速。
可见主机负载波动，所有历史运行保留；本表采用最终交错运行，未采用早期顺序分组测量。

原始记录：`target/dscslide-verified/report.json`（含每个调用及二进制 SHA-256）。
最终二进制 SHA-256：`1dc13b0b830b7f3f3ac019ff40bb54f24ab80f4cae377df92fdc327b4631c0dc`。
基线二进制：`target/dscslide/attesor-baseline`。

## 4. 正确性与回退检查

`tests/attesor/dscslide.c` 同时构建 x86_64 native 和 arm64 guest；输出均为：

```text
slide-42 length=8 number=0
```

缓存禁用、冷缓存、热缓存，以及损坏/版本不匹配/截断后的回退/修复均得到相同输出。
在 dyld/客体修改数据之前，直接比较 **36 个 16 KiB dyld 页**的实际字节，
覆盖 12 个包含实际滑动指针的映射（每个选首/中/末含指针页）。
样本文件含记录头共 590,688 字节；所有模式逐字节相等，而非只检查客体输出。
样本 SHA-256：`d14d5596812ae47dbc4b61d371c311da2cea4a01f0f7703ff02c6bb32074f7e4`。

还通过：完整 checksum 损坏、旧格式、截断、锁被占用、缓存路径不是目录、
四个同时启动的进程恰好一位发布者、后续热命中、touch-on-use、AOT 启用时的冷/热共存。
所有子进程输出和 `.pages` 文件均保留在 `target/dscslide-verified/`。

复现（输出目录必须不存在）：

```bash
cargo build --release --workspace
PYTHONDONTWRITEBYTECODE=1 python3 tests/attesor/dscslide-check.py \
  --attesor target/release/attesor \
  --baseline target/dscslide/attesor-baseline \
  --output target/dscslide-reproduce
```

## 5. 测试结果与完整自测行

```text
cargo build --release --workspace: PASS
cargo test --release --workspace: PASS (attesor 64 + core 9; difftest/doc-tests 0)
Python dscslide-check.py syntax compilation: PASS
dscslide-check.py correctness/concurrency/fallback/startup checks: PASS
AOT checkpoint persistence/repair/failed-exec/multithreaded-fork: PASS
Profiler workload/interpreter/multithreaded-fork/exec/failed-exec/SIGKILL snapshot: PASS
Profiling-enabled independent parent/child fork snapshots: PASS
git diff --check: PASS
```

新增 7 项 Rust 测试覆盖 v3/v5 指针结果、跨页/合并脏页、key 全字段、头/长度/校验/extent 验证、
稀疏 MAP_PRIVATE 与 touch、部分覆盖失败的全量恢复、非阻塞锁与 tidy（旧版本/30 天/死写者）。
`cargo test --release --workspace` 已在默认并发模式重复通过。
测试专用等待处理现有 raw-fork JIT fault 测试短暂继承 fd 的情况，生产写锁始终非阻塞。

额外 profiler 全套第一次因采样没有捕获 `libsystem_c` 而失败；未修改断言或 profiler，
原基线和最终二进制后续完整运行均通过；首次失败日志也保留，不能把这个统计采样断言当作确定性覆盖。
`rustfmt` 组件未安装，未声称通过 cargo fmt；构建保留项目原有编译警告。

完整 28 组差分自测，全部 `mismatch=0`，并检查 native（或 insns > fallback_insns）非零。
blocks=300,000；fuse/fuse2/selchain=200,000；其他常规类及 FP mask=100,000；crypto=12,000。
原始命令、环境、输出见 `target/dscslide/selftests.json` 和 `selftests.log`：

```text
{} jit selftest [blocks]: ok=26375 mismatch=0 skipped=273625 insns=90648 fallback_insns=3480
{} jit selftest [fuse]: ok=126471 mismatch=0 skipped=73529 native=126471 pairs=126471 dead_flag_skips=0
{} jit selftest [fuse2]: ok=65395 mismatch=0 skipped=134605 native=65395 pairs=65395 dead_flag_skips=69471
{} jit selftest [selchain]: ok=72305 mismatch=0 skipped=127695 native=72305 dead_flag_skips=53373
{} jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
{} jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
{} jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
{'ATTESOR_SELFTEST_MEM': '1', 'ATTESOR_SELFTEST_SIMDMEM': '1'} jit selftest [ldst]: ok=53684 mismatch=0 skipped=46316 native=44252
{} jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
{} jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49451
{} jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
{} jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
{} jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
{} jit selftest [ff000000:1f000000]: ok=75018 mismatch=0 skipped=24982 native=50098
{} jit selftest [bf20fc00:0e20cc00]: ok=74862 mismatch=0 skipped=25138 native=74862
{} jit selftest [9f800400:0f000000]: ok=28156 mismatch=0 skipped=71844 native=4576
{'ATTESOR_SELFTEST_FPCR': '1'} jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
{'ATTESOR_JIT_ON': 'callret'} jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=1 deep=12500
{'ATTESOR_JIT_OFF': ''} jit selftest [ffffcc00:4e284800]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': ''} jit selftest [bfe0fc00:0ee0e000]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': ''} jit selftest [ffe0ec00:5e004000]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': ''} jit selftest [fffffc00:5e282800]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': ''} jit selftest [ffe0fc00:5e006000]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': 'shani'} jit selftest [ffffcc00:4e284800]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': 'shani'} jit selftest [bfe0fc00:0ee0e000]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': 'shani'} jit selftest [ffe0ec00:5e004000]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': 'shani'} jit selftest [fffffc00:5e282800]: ok=12000 mismatch=0 skipped=0 native=12000
{'ATTESOR_JIT_OFF': 'shani'} jit selftest [ffe0fc00:5e006000]: ok=12000 mismatch=0 skipped=0 native=12000
```

其他日志：`target/dscslide/cargo-test-final.log`、`cargo-test-repeat.log`、
`aot-checkpoint.log`、`profile-check-final.log`、`profile-fork.log`。

## 6. 提交信息

`target/dscslide-commit-message.txt`

没有写入 Git 元数据或创建提交：这个 linked worktree 的 Git 元数据位于任务明确禁止触碰的
`~/attesor/.git`。按任务的受限 Git 规则，将工作留在当前树中，并保存提交信息；未推送。
