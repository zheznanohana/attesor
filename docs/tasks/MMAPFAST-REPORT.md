# MMAPFAST 实现与验证报告

## 范围与限制

- 工作分支：`mmapfast`；基线：`9cb50b6`。未推送，修改留在本工作树。
- DeepSeek Harness 只通过 `ELECTRON_RUN_AS_NODE=1` 执行，没有启动 GUI。
- **完整 Node 工作负载尚未验证**：原样运行 `docs/tasks/node-workload.js`，文件遍历完成（14,499 个条目），随后沙箱拒绝 HTTP `listen(0)`，报 `EPERM: operation not permitted 0.0.0.0`，退出码为 1。没有修改工作负载或绕过网络限制。下面的实测数据只覆盖该退出点之前的工作，不可与任务提供的完整运行混同。
- 临时文件、HOME、AOT 输出和性能记录均重定向到 `target/mmapfast/`；系统 arm64 sysroot 和应用资源只读使用。

## 修改的函数

- `syscalls::mmap_16k` / `mmap_16k_with`：非固定、非零长度请求先直接映射 16 KiB 向上取整的长度；已对齐立即返回。未对齐则解除首次映射，再走原有预留、固定覆盖、前后裁剪流程；失败映射保留错误并清理预留区。对长度取整溢出返回 `EINVAL`。
- 仅无调用者 hint 时，使用上次成功的对齐区间末尾作为普通地址 hint；不是预留或固定映射。显式 hint 优先，失败不推进 hint，零长度和 `MAP_FIXED` 完全原样转发。
- `bsd_inner` 中 `PROT_EXEC` / `MAP_JIT` 剥离，以及可执行文件映射的符号 / AOT 注册代码未改动。
- `profile::Active::host_begin/host_end`：抽出原有计时操作以便确定性测试；`HostGuard` 和 fork 计时共用它们。**原实现已通过 `host_ns += ...` 累加多个宿主调用，并不存在“只计最后一次”的错误，因此没有改动计时归属语义。** `hostsys::raw` 的边界也未改动。
- `profile::mmap_fast_path`、`Data::resume/write`、`ForkGuard::child`：仅 profiling 开启时记录首次尝试次数与对齐成功次数；exec 保留累计值，fork 子进程清零。关闭 profiling 时只有 enable 检查。
- `tools/profile-report.py` 的 `Report`、`read_profile`、`render`：兼容原 v1/v2，合并并显示命中率；分母包括失败的直接尝试，不包括固定、零长度或尚未尝试就因长度溢出被拒绝的请求。

## 测试覆盖

- `crates/attesor/src/syscalls/mmap_tests.rs`：真实 Darwin 映射通过 `handle_svc` 进入，覆盖 12 种匿名长度、9 种文件映射长度，包含 4 KiB / 16 KiB 边界和大于 1 MiB 的长度；私有 / 共享、零 / 对齐 / 4 KiB 非对齐 hint、保护位和 `MAP_JIT` 剥离、16 KiB 舍入尾部的读写、文件偏移与内容、COW / 共享写入、固定映射相邻哨兵、无效 fd / 偏移 / 固定地址等错误。
- 脚本化宿主结果覆盖一次调用返回、4 种预留基址对齐情况、首次映射的解除、双侧裁剪、预留失败、固定覆盖失败后的完整清理，以及 hint 的优先级、推进和失败保持。
- 保留 Darwin 原始 syscall 的零长度语义：它可以成功，不假设 libc 的 `EINVAL` 行为。
- `profile::call_tests::multiple_host_calls_accumulate_instead_of_counting_only_the_last`：确定性验证四个宿主区间相加，以及第五个活动区间的快照与完成不重复计数。
- `profile::call_tests::snapshot_round_trip_preserves_calls_and_v1_remains_readable` 和 `tests/attesor/profile-report-check.py`：命中计数持久化、加权合并、旧格式兼容和非法计数拒绝。

## 复现命令

构建与工作区测试使用离线依赖，避免下载；同样执行了串行测试：

```sh
export TMPDIR="$PWD/target/mmapfast/tmp"
cargo build --release --workspace --offline
cargo test --release --workspace --offline
cargo test --release --workspace --offline -- --test-threads=1
python3 tests/attesor/profile-report-check.py
python3 tests/attesor/profile-syscalls-check.py --output target/mmapfast/profile-syscalls
python3 tests/attesor/profile-check.py --output target/mmapfast/profile-lifecycle
```

Node 每次使用新的输出目录；基线二进制在修改前保存为 `target/mmapfast/attesor-before`：

```sh
ELECTRON_RUN_AS_NODE=1 \
RES="/Applications/DeepSeek Harness.app/Contents/Resources" \
ATTESOR_PROFILE="$OUT/profile" ATTESOR_CACHE="$OUT/aot" ATTESOR_NO_AOT_BUILD=1 \
ATTESOR_SYSROOT="/Users/USER/attesor-sysroot/26.6.2__Mac14,2" \
TMPDIR="$OUT/tmp" HOME="$OUT/home" \
target/release/attesor "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness" \
"$PWD/docs/tasks/node-workload.js"
python3 tools/profile-report.py "$OUT/profile"
```

具体启动器、超时、退出码与全部日志保存在 `target/mmapfast/run-node.py` 和各次运行的目录中。

## 实测结果：原样 Node 工作负载的受限前缀

计时核查没有发现漏计的连续宿主区间。任务附带的完整历史运行是：`mmap` 3,245 次，1,198.919 / 47.431 / 1,151.488 ms（总 / 宿主 / 仿真）；不能把其中的仿真时间自动改归为宿主时间。本环境 HTTP 被拒绝，无法重获完整运行的 split。

最终测量在长时间 JIT 自测结束后进行，基线与最终实现交替运行三轮，每次使用全新的私有目录。以下为**三轮合计**，均正常记录退出码 1 和 `listen EPERM`；不是完整 HTTP 工作负载的结果。

| 调用 | 修改前次数 | 前总/宿主/仿真 ms | 修改后次数 | 后总/宿主/仿真 ms |
|---|---:|---|---:|---|
| mmap | 564 | 741.153 / 167.377 / 573.776 | 549 | 119.033 / 83.676 / 35.356 |
| munmap | 369 | 8.759 / 8.559 / 0.199 | 355 | 6.371 / 6.102 / 0.269 |
| mprotect | 4888 | 42.440 / 35.669 / 6.771 | 4878 | 39.926 / 33.660 / 6.266 |
| mach_vm_allocate | 237 | 3.085 / 2.813 / 0.273 | 237 | 3.123 / 2.964 / 0.159 |
| mach_vm_map | 111 | 0.770 / 0.655 / 0.115 | 113 | 0.776 / 0.640 / 0.136 |

注意：第一轮基线 mmap 的仿真时间有明显离群值（541.108 ms）。不据三轮总时间宣称固定倍数的整体加速。mmap 总时间的三轮中位数为修改前 72.902 ms、修改后 36.404 ms；宿主时间中位数为 55.256 / 23.042 ms。端到端耗时分别为修改前 12.713、11.228、10.813 s，修改后 10.946、10.909、12.022 s；该短前缀的总墙钟时间没有稳定改善。

### 命中率与 hint 决策

- 不带自适应 hint 的直接尝试，三轮为 `39/126`、`39/126`、`38/126`，合计 **116/378 = 30.69%**。低命中率会带来解除首次映射的额外调用，因此才加入对齐末尾 hint。
- 最终实现三轮为 `59/126`、`123/126`、`125/127`，合计 **307/379 = 81.00%**。只有首次成功且地址 16 KiB 对齐才算命中；失败尝试也在分母中。
- ASLR、现有映射和调用者显式 hint 会影响结果；上述命中率只代表受限前缀，不外推至尚未运行的 HTTP 阶段。

### 其余内存调用

代码审计：`mach_vm_allocate` 首次建立 arena 时可能有多次已计时的 mmap/munmap，随后通常为一次原始 Mach trap；`mach_vm_map` 和 `munmap` 直接转发；`mprotect` 转发后仅在请求执行权限时做既有失效处理。没有同样的逐次预留/裁剪模式。

| 调用 | 修改前仿真 µs/次 | 修改后仿真 µs/次 |
|---|---:|---:|
| mach_vm_allocate | 1.151 | 0.673 |
| mach_vm_map | 1.040 | 1.207 |
| munmap | 0.540 | 0.758 |
| mprotect | 1.385 | 1.285 |

这些函数未改动，以上差异只是运行波动；未发现值得额外优化的明确高成本，故没有扩大修改范围。

### 各轮原始数据

| 运行 | 调用 | 次数 | 总 ms | 宿主 ms | 仿真 ms | 仿真 µs/次 |
|---|---|---:|---:|---:|---:|---:|
| final-before-1 | mmap | 187 | 604.937 | 63.828 | 541.108 | 2893.628 |
| final-before-1 | munmap | 120 | 2.814 | 2.747 | 0.067 | 0.556 |
| final-before-1 | mprotect | 1621 | 14.331 | 12.173 | 2.157 | 1.331 |
| final-before-1 | mach_vm_allocate | 79 | 1.074 | 1.006 | 0.068 | 0.864 |
| final-before-1 | mach_vm_map | 32 | 0.259 | 0.221 | 0.037 | 1.157 |
| final-after-1 | mmap | 183 | 51.431 | 40.026 | 11.405 | 62.322 |
| final-after-1 | munmap | 118 | 2.203 | 2.144 | 0.059 | 0.497 |
| final-after-1 | mprotect | 1617 | 12.978 | 10.873 | 2.106 | 1.302 |
| final-after-1 | mach_vm_allocate | 79 | 0.999 | 0.947 | 0.052 | 0.659 |
| final-after-1 | mach_vm_map | 33 | 0.251 | 0.215 | 0.036 | 1.091 |
| final-before-2 | mmap | 188 | 72.902 | 55.256 | 17.647 | 93.865 |
| final-before-2 | munmap | 124 | 3.509 | 3.432 | 0.077 | 0.623 |
| final-before-2 | mprotect | 1644 | 14.385 | 12.122 | 2.263 | 1.377 |
| final-before-2 | mach_vm_allocate | 79 | 1.093 | 1.042 | 0.051 | 0.643 |
| final-before-2 | mach_vm_map | 47 | 0.305 | 0.261 | 0.044 | 0.936 |
| final-after-2 | mmap | 183 | 36.404 | 23.042 | 13.362 | 73.018 |
| final-after-2 | munmap | 118 | 2.319 | 2.236 | 0.083 | 0.705 |
| final-after-2 | mprotect | 1640 | 13.211 | 11.301 | 1.910 | 1.165 |
| final-after-2 | mach_vm_allocate | 79 | 1.019 | 0.975 | 0.044 | 0.559 |
| final-after-2 | mach_vm_map | 32 | 0.290 | 0.221 | 0.070 | 2.176 |
| final-before-3 | mmap | 189 | 63.314 | 48.293 | 15.021 | 79.478 |
| final-before-3 | munmap | 125 | 2.435 | 2.380 | 0.055 | 0.442 |
| final-before-3 | mprotect | 1623 | 13.724 | 11.374 | 2.350 | 1.448 |
| final-before-3 | mach_vm_allocate | 79 | 0.918 | 0.764 | 0.154 | 1.945 |
| final-before-3 | mach_vm_map | 32 | 0.207 | 0.172 | 0.034 | 1.075 |
| final-after-3 | mmap | 183 | 31.197 | 20.608 | 10.589 | 57.865 |
| final-after-3 | munmap | 119 | 1.849 | 1.721 | 0.127 | 1.071 |
| final-after-3 | mprotect | 1621 | 13.737 | 11.487 | 2.250 | 1.388 |
| final-after-3 | mach_vm_allocate | 79 | 1.106 | 1.042 | 0.063 | 0.800 |
| final-after-3 | mach_vm_map | 48 | 0.235 | 0.204 | 0.031 | 0.641 |

每轮完整系统调用表、原始 profile、Node 输出和退出状态均在 `target/mmapfast/final-before-{1,2,3}/`、`target/mmapfast/final-after-{1,2,3}/`。不带 hint 的对照为 `target/mmapfast/after-{1,2,3}/`。汇总脚本为 `target/mmapfast/summarize.py`。

## 实际测试输出

最终原样命令 `cargo test --release --workspace`（另有 offline 和串行运行，均通过）：

```text
test result: ok. 57 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.09s
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

新增 mmap / 多宿主区间回归测试的实际输出：

```text
test profile::call_tests::multiple_host_calls_accumulate_instead_of_counting_only_the_last ... ok
test syscalls::mmap_tests::aligned_end_hint_is_optional_nonfixed_and_updated_only_on_success ... ok
test syscalls::mmap_tests::failures_propagate_and_failed_remap_releases_entire_reservation ... ok
test syscalls::mmap_tests::first_try_preserves_hint_flags_offset_and_returns_after_one_call ... ok
test syscalls::mmap_tests::real_fixed_does_not_round_length_or_replace_its_neighbours ... ok
test syscalls::mmap_tests::unaligned_first_try_is_unmapped_then_reserve_and_both_slack_sides_are_trimmed ... ok
test syscalls::mmap_tests::zero_length_and_fixed_are_forwarded_unchanged ... ok
test syscalls::mmap_tests::real_file_backed_private_shared_offsets_hints_and_failures ... ok
test syscalls::mmap_tests::real_anonymous_sizes_hints_protections_and_rounded_tails ... ok
```

JIT 自测使用既有完整列表，每类 100,000 次，分别在默认配置和 `ATTESOR_JIT_OFF=loopra/vdead/callret` 下执行：

```sh
for E in "" loopra vdead callret; do
  for c in blocks fuse fuse2 selchain fpblocks dpimm dpreg ldst branch loops fpnan tbiblocks calls; do
    ATTESOR_JIT_OFF="$E" target/release/attesor --selftest-jit "$c" 100000
  done
done
```

52 条命令全部退出 0，全部 `mismatch=0`；同时检查 `native>0`，或 block 类的 `insns>fallback_insns`，避免解释器回退造成空验证。首次临时驱动器误要求 block 类也打印 `native=`，造成 8 个格式误报；`target/mmapfast/audit-selftests.py` 对全部原始结果和返回码复核通过，修正后的驱动器已保存，未把误报当作产品测试失败或隐藏实际输出。

```text
ATTESOR_JIT_OFF='' jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
ATTESOR_JIT_OFF='' jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
ATTESOR_JIT_OFF='' jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
ATTESOR_JIT_OFF='' jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
ATTESOR_JIT_OFF='' jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
ATTESOR_JIT_OFF='' jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
ATTESOR_JIT_OFF='' jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
ATTESOR_JIT_OFF='' jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
ATTESOR_JIT_OFF='' jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
ATTESOR_JIT_OFF='' jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
ATTESOR_JIT_OFF='' jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
ATTESOR_JIT_OFF='' jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
ATTESOR_JIT_OFF='' jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
ATTESOR_JIT_OFF='loopra' jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
ATTESOR_JIT_OFF='loopra' jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
ATTESOR_JIT_OFF='loopra' jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
ATTESOR_JIT_OFF='loopra' jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
ATTESOR_JIT_OFF='loopra' jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
ATTESOR_JIT_OFF='loopra' jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
ATTESOR_JIT_OFF='loopra' jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
ATTESOR_JIT_OFF='loopra' jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
ATTESOR_JIT_OFF='loopra' jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
ATTESOR_JIT_OFF='loopra' jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=0
ATTESOR_JIT_OFF='loopra' jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
ATTESOR_JIT_OFF='loopra' jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
ATTESOR_JIT_OFF='loopra' jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
ATTESOR_JIT_OFF='vdead' jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
ATTESOR_JIT_OFF='vdead' jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
ATTESOR_JIT_OFF='vdead' jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
ATTESOR_JIT_OFF='vdead' jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
ATTESOR_JIT_OFF='vdead' jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
ATTESOR_JIT_OFF='vdead' jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
ATTESOR_JIT_OFF='vdead' jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
ATTESOR_JIT_OFF='vdead' jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
ATTESOR_JIT_OFF='vdead' jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
ATTESOR_JIT_OFF='vdead' jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
ATTESOR_JIT_OFF='vdead' jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
ATTESOR_JIT_OFF='vdead' jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
ATTESOR_JIT_OFF='vdead' jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
ATTESOR_JIT_OFF='callret' jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
ATTESOR_JIT_OFF='callret' jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
ATTESOR_JIT_OFF='callret' jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
ATTESOR_JIT_OFF='callret' jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
ATTESOR_JIT_OFF='callret' jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
ATTESOR_JIT_OFF='callret' jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
ATTESOR_JIT_OFF='callret' jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
ATTESOR_JIT_OFF='callret' jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
ATTESOR_JIT_OFF='callret' jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
ATTESOR_JIT_OFF='callret' jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=49340
ATTESOR_JIT_OFF='callret' jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
ATTESOR_JIT_OFF='callret' jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
ATTESOR_JIT_OFF='callret' jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
PASS: 52/52 commands returned 0; all mismatch=0; native>0 or insns>fallback_insns.
```

Profiler 专项检查：

```text
profile-report: PASS lossless merge, weighted shares, zero samples, process roles
profile-report: PASS v1/v2, syscall time splits, zero-sample idle calls, selector names across processes, invalid counters
profile-syscalls: PASS iterations=20000 checksum=340705764445374980
profile-syscalls-check: PASS syscall counts, indirect getpid, kernel/emulation split, idle sleep and zero-timeout kevent
profile-syscalls-check: PASS same workload with profiler disabled
profile-check: PASS main/libsystem_c/libz symbols, JIT/interpreter, idle sleep
profile-check: PASS --interp has no JIT samples
profile-check: PASS multithreaded fork has independent samples/calls/times in child and parent
profile-check: PASS same-pid exec preserves pre-exec samples/calls/times and inherited setting
profile-check: PASS failed exec records the returned host interval once
profile-check: PASS 10s atomic snapshot includes an active host wait and survives SIGKILL
```

`git diff --check`、修改的 Python 文件语法编译也通过；Python 编译缓存重定向至 `target/mmapfast/pycache`。构建仍有既有警告，未在本任务扩大清理范围。

## 提交信息与产物

Git 元数据在当前权限下只读，未尝试写入共享主工作树的 Git 元数据，也未推送。提交信息已保存到：

`target/mmapfast-commit-message.txt`

基线二进制 SHA-256：`7e5a9a71643baf3b752c337ae348a17b65b2d73dd3af846b16ab481751e5b9ab`。
最终二进制 SHA-256：`f63fea1c8ed9aa842cc0da5dc10f55420e5bdd3e3ad6c86c236ca58865105817`。
