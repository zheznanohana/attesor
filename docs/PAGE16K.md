# 实验性 16 KiB syscall 范围

默认关闭。`ATTESOR_JIT_ON=page16k` 精确启用，逗号分隔；
`ATTESOR_JIT_OFF=page16k` 优先关闭。`page16k-extra`、空值或仅 OFF 不会启用。
配置按进程首次使用固定，适用于 JIT 和解释器；沿用已有子进程环境继承。

启用时，commpage 的用户/内核 page shift 与数字 `CTL_HW/HW_PAGESIZE`
查询均报告 16 KiB，使 `vm_page_size`、`host_page_size()`、`getpagesize()` 和
`sysconf(_SC_PAGESIZE)` 与下述对齐策略一致。默认和 OFF 仍保留原有 4 KiB 行为。

范围处理只归一化可表示的非零 `munmap`、`mprotect`、`madvise`：

- `munmap` / `mprotect` 起始地址必须 16 KiB 对齐，否则在宿主调用前返回 `EINVAL`。
- 可表示的范围尾部向上覆盖至 16 KiB；`madvise` 起点向下覆盖至 16 KiB。
- 成功的 executable `mprotect` 沿用现有失效接口，但传入完整归一化范围，包含舍入尾部。
- 其余保护位、advice 和宿主返回值仍由既有路径处理，不缓存、合并或省略合法宿主操作。

零长度完全沿用原始路径。地址加长度或尾部取整溢出时不修改参数、不自行推断 errno，
仍由原始宿主路径裁决；因此这不是所有边界的完整 guest VM 模型。
`mmap`（包括 `MAP_FIXED`、offset、零长度）、Mach VM 调用和代码生成均不改动。
不新增 `munmap` / destructive `madvise` 的翻译失效策略。

## 语义依据

本机 SDK：`/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk/usr/share/man/man2/` 下
`munmap.2`、`mprotect.2` 的对齐约束，及 `mmap.2` 的整页扩展说明。
仓库 `ref/xnu_main_bsd_kern_syscalls.master:131-133` 给出 syscall ABI，
`ref/vm_unix.c:1260-1275` 展示 map 自身页粒度，不把 host 页粒度当作 guest 页粒度。

Apple 的固定版本源码说明对齐检查和整页范围转换：
[kern_mman.c](https://github.com/apple-oss-distributions/xnu/blob/xnu-8796.121.2/bsd/kern/kern_mman.c#L971-L1031)、
[vm_user.c](https://github.com/apple-oss-distributions/xnu/blob/xnu-8796.121.2/osfmk/vm/vm_user.c#L393-L412)、
[protection](https://github.com/apple-oss-distributions/xnu/blob/xnu-8796.121.2/osfmk/vm/vm_user.c#L501-L524)、
[advice](https://github.com/apple-oss-distributions/xnu/blob/xnu-8796.121.2/osfmk/vm/vm_user.c#L1667-L1702)。
reusable advice 可使用硬件页粒度；这里的目标 guest 假设为 16 KiB arm64 页，
不是任意软件页大小 / 4 KiB-compat task 支持声明。

仅完成源码实现与离线编译检查；未执行行为、性能、应用或硬件验收。
没有已证明的性能收益，不能据此默认开启。

## integ1 移植验收

上述“仅编译”是原功能分支交付时的历史状态。2026-10-30 的 integ1 移植补充了范围边界、
真实宿主页尾/相邻页与恢复式变异测试；保留当前 `jit::invalidate_for` 接线。
实际结果与限制见 [PORT-G4-REPORT.md](PORT-G4-REPORT.md)，仍不宣称完整 VM 语义或性能收益。

## L1 集成修正

原移植遗漏了页大小发现路径：开启范围策略但仍报告 4 KiB，导致 headless
Electron/Node 分配器收到 `EINVAL` 并执行 BRK。集成修正只在 `page16k` 下统一
上述发现路径，未放宽对齐检查。`tests/attesor/page16k-check.py` 覆盖真实 libc、
解释器/JIT、默认/ON/OFF 优先/伪 token，以及相邻页保护。
最终矩阵与网络验证限制见 [INTEGRATION-REPORT.md](INTEGRATION-REPORT.md)。
