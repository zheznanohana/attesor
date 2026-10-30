# integ1 · Port G4（2026-10-30）

## 范围与接入

按 **#31 → #5 → #7 → #33 → #28** 顺序手工移植指定 production patches。
原树已有大量暂存的 wave-1/G2/G3 成果，全部保留；未 stage、commit、reset、stash，未写 Git 索引或引用。
未跳过任何编号：五项的有限生产子集在当前树仍有实际接入点，但不等于完成各编号的全部性能目标。

所有新增特性默认关闭，使用精确逗号 token，匹配的 OFF 优先：
`hugepages`、`flagreg`、`isel2`、`page16k`、`sharedtrans`。
原始 ON/OFF/DISABLE 字符串继续参与 codegen identity；共享翻译 key 同时保存完整字符串。
现有 AOT flags 0–10 不动，新分配 flagreg **11**、BMI2/LZCNT **12/13**、hugepages 请求策略 **31**。
页语义与共享 token 通过原始选项 identity 隔离；实际 ISA 选择另有有效 flags，不能只依赖 ON 字符串。
五项完成后统一将 `TRANSLATOR_VERSION` **63 → 64**，没有套用旧补丁的 53/54/55。

## 逐项结果

| 编号 | 当前树适配 | 每项 release build / L0 / self-test |
|---|---|---|
| #31 hugepages | JIT/AOT 共用 allocator，精确 2 MiB Mach 请求，拒绝/保护失败释放后回退原 RWX mmap；保留现有 arena 边界 | 通过；2 项 allocator 定向测试 × default/ON/ON+OFF/非精确 token；blocks 1000，mismatch=0 |
| #5 flagreg | 有界单块 loopra CMP+CB/TB 私有回边，公共出口重算并物化 NZCV；补上当前 `emit_block_inner` 的状态重置，明确排除 loopra2 多块私有标签 | 通过；2 项定向测试 × 四种开关；blocks、loops 各 1000，mismatch=0 |
| #7 isel2 | BMI2 变量移位和 LZCNT CLZ/CLS；共用发码/缓存 ISA 快照，避开既有 AVX512 标志位 | 通过；W/X、所有源/目标别名与 ZR、count 边界、region/non-region、Plain/Patchable、NZCV 对照解释器；dpreg、blocks、loops 各 1000，mismatch=0 |
| #33 page16k | 接入现有 BSD 公共入口；归一化 munmap/mprotect/madvise。当前 mprotect 已调用 `jit::invalidate_for`，保留该接口与 preciseinvalidate 策略，不退回旧 engine 接口 | 通过；2 项范围/真实 syscall 测试 × 四种开关，验证宿主页尾、相邻页、拒绝不对齐、零长度和溢出透传 |
| #28 sharedtrans | 接入当前 BlockRegistry / UnitRec，填写 host_bytes、精确 source_ranges 和 source pages；保留 INDEX→BLOCKS 锁序与 AOT 竞争发布检查，不把未守卫的 Plain entry 暴露给 rel32chain | 通过；定向源码快照、PI/指令策略、本地执行、incoming 撤链/epoch、损坏/identity/容量、独立进程 producer/consumer；blocks、loops 各 1000，mismatch=0 |

#5 的执行测试覆盖 W/X、LSL #12、carry/overflow/符号边界、CMP Rn 与分支寄存器别名、CB/TB、私有回边和公共出口；proof 测试拒绝 helper、内存、FP、flag observer、非法 MOV、SP、公开标签和长度截断。
#7 在本机 ON 下实际选择 **BMI2=true、LZCNT=true**，不是仅测试 fallback；每轮定向执行 10,080 个组合。

#28 与 tiered JIT 同开时保守使用已有 cold/full/promotion 路径，不尝试共享；VERIFY/STATS/PROFILE 与 jitcost probes 也回退，避免共享诊断状态及探针。
不改变原来 AOT 外部登记、alive guard、后台翻译、缓存生命周期或 guest fork 的合约。

## #28 运行中发现并修复的问题 / 环境限制

原补丁给 Darwin `shm_open` 传入 `O_CLOEXEC`，本机探针返回 **EINVAL (22)**。
改为使用 SDK 列出的 shm_open flags，并在使用 fd 前 `F_GETFD/F_SETFD(FD_CLOEXEC)`，失败关闭 fd 后回退。
本机 SDK `MacOSX26.5.sdk/usr/share/man/man2/shm_open.2` 也说明返回 fd 自带 FD_CLOEXEC。
去掉不支持的 flag 后，本受管环境创建 POSIX shm 仍返回 **EPERM (1)**；没有提升权限或绕过该限制。
原始记录：`target/port-g4/logs/28-posix-shm-probe.log`、`28-diagnose.log`。

独立 exec 的 producer/consumer 因此使用 **仅 `cfg(test)` 存在的私有工作树文件 backing**。
这条测试仍经过生产的长度/owner/mode/header 检查、原子发布/读取、完整 key/source/code 验证、
`translate_inner → prepare → register_plain_unit` 和真实 native 执行；consumer 断言 adopted=1、local_plain=0，且与 producer 的本地 host 地址不同。
测试文件由 RAII 删除，不碰 `/attesor-st1-<uid>` 的生产池。非测试二进制不识别测试文件/名称 override。
**文件 backing 下的协议/翻译器验证不等于真实命名 POSIX shm 已验收**；真实 shm attach、fork/崩溃/并发压力仍需在允许的环境另验。

## 敏感性证明

每项均执行源码变异，要求测试运行并报告 FAILED（编译错误不算），随后 finally 恢复原文并重跑通过：

- #31：删除被拒绝 Mach allocation 的释放调用，release 次数断言失败。
- #5：删除公共出口 NZCV 修复，解释器对照失败。
- #7：初验错误移位方向失败；最终另将 SHLX 的 value/count 互换，**同一 mnemonic** 下 GPR 语义对照失败，排除仅靠 opcode 形状报错的弱证明。
- #33：保留原长度、不扩展页尾，范围和真实 VM 尾页断言失败。
- #28：跳过采用前 source 重读比较，stale-source 断言失败；正常 fixture 还明确展示保留旧 host code 返回 12，而更新后的 guest 应返回 47。

没有遗留源码变异。逐项原始记录为 `target/port-g4/logs/{31,5,7,33,28}-{gate,mutant,restored}.log`，另有 `7-semantic-{mutant,restored}.log`。

## 最终验收

- 最终 `cargo build --release -p attesor` 通过。
- `cargo test --release --workspace -- --test-threads=1`：attesor **136** 项、core **9** 项通过，difftest 无单测。
- `flagreg,isel2,page16k,sharedtrans` 同开：attesor **136** 项通过。
- 上述四项 + tiered/bgtrans/fastenc/preciseinvalidate/slotreuse/jitcost/bliccache/ibtc2/rel32chain/notbi：**136** 项通过。
- 新 token 全部 ON+OFF 的 `g4_` 定向 **13** 项通过；flagreg + directbranch OFF、sharedtrans + preciseinvalidate/rel32chain/fastenc、sharedtrans + jitcost 回退以及三种诊断开关策略门禁通过。
- hugepages ON 仅运行小分配/回退测试及不分配 code cache 的 identity 测试；**没有把生产 4 GiB 大页请求混入组合执行门禁**。
- 同一测试二进制 default、五个单独 token、五项全 ON、全 ON+OFF 得到 **8 个不同 codegen/AOT identity**；逐位破坏 flags 11/12/13/31 的文件头均被兼容性检查拒绝。
- 四个执行相关新 token 同开，dpreg/blocks/fpblocks/loops/calls/jitcode/offline 各 1000 次抽样/请求，全部 mismatch=0。
  dpreg 实际有效 306、跳过 694；blocks 实际有效 103、跳过 897；其余各 1000 有效。
  offline 实际 disk_loaded_blocks=6510。该测试不是共享翻译性能或命中率评估。
- Python 编译检查（pycache 指向 target）与 `git diff --check` 通过。

完整最终记录：`target/port-g4/logs/final-gates.log` 和对应 `final-*.log`。
过程中修复了测试 fixture 的 Rust 类型/借用错误、误用的 self-test 类名（改为 dpreg），
以及组合门禁中错误地要求含 jitcost probes 的 Plain code 仍可共享的测试前提；
现在测试明确验证该场景保守回退，而不放宽共享代码允许访问的 Cpu 字段。

## 可复跑入口

```sh
# 按指定顺序逐项 build / L0 / self-test / 源码恢复式敏感性测试
python3 tests/attesor/port-g4-check.py all --sensitivity
# 最终 workspace、组合、AOT identity 与同 mnemonic 语义敏感性门禁
python3 tests/attesor/port-g4-check.py all --final
```

串行运行，不要与编辑或其他构建同时执行。测试使用本树 `target/port-g4/cache`，不安装应用、不操作 GUI、不使用用户 AOT cache。
日志：`target/port-g4/logs/`。提交说明文本：`target/port-g4-commit-message.txt`，只写文件，不创建提交。

## 未声称完成

没有性能收益结论；#31 未申请/验收生产 4 GiB superpage arena，也没有物理大页 backing 证据（仅定向 mock 与小 mmap 回退）。
#5 不是通用跨块 live NZCV；#7 未实机模拟缺 BMI2/LZCNT 的另一台 CPU；#33 不是完整 guest VM 语义。
#28 保持固定容量、同 UID 可信 producer、append-only code、无 eviction/自动清理/恶意同 UID 防护。
未解决通用并发 SMC、全部 AOT 登记生命周期、多线程 guest fork 或信号重入；测试不能替代这些完整性证明。
