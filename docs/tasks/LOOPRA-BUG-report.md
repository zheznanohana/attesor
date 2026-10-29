# Loopra 静默误编译调查与修复

## 已证实的根因

`emit.rs::E::ldst` 的区域寄存器偏移快路径将 SIMD Q 的 ARM 缩放 `#4`
转换为 `LEA base + index * 16`。x86 SIB 只能编码 ×1、×2、×4、×8。
当前 iced-x86 版本在 debug 构建中断言失败；release 中实际编码成 ×8，
所以合法的 `LDR/STR Q [Xn, Xm, LSL #4]` 和 `[Xn, Wm, UXTW #4]`
会静默访问错误地址。标量的最大缩放为 ×8，SXTW/SXTX 走另一条显式移位路径，
这解释了旧的 loops 生成器（寄存器偏移只生成标量 LSL）为什么一直通过。

已确认这是一个真实的内存读写误编译；没有运行用户应用或修改应用/共享缓存，因而不声称已经
在 DeepSeek Harness、VS Code 或真实 TLS 路径上验证恢复。

## 失败与缩减

- 扩展生成器但尚未加入 Q 寄存器偏移时：`0x1234` 的 20,000 例以及
  `0xdeadbeef` 的 100,000 例均通过。加入 Q 后，`loops 10000 0x1234`
  出现 **118** 个 mismatch；同一批 `ATTESOR_JIT_OFF=loopra` 为 **0**。
- 最早失败是最大块长 case 0（其中 `STR Q19 [X22,W29,UXTW #4]` 写错位置）；
  更短的 case 89 含九条循环指令（含回边控制），其中 `3cbd7a82` 是
  `STR Q2,[X20,X29,LSL #4]`，写错位置又污染了后续标量加载。
- 去掉算术、标量加载、SIMD 清零和 NEG/TBNZ 控制，将它缩为：

  ```text
  3cbd7a80    str q0,[x20,x29,lsl #4]
  f100079c    subs x28,x28,#1
  54ffffc1    b.ne loop
  d4001001    svc #0x80
  ```

  令 index=3、counter=1 或 2、Q0 为固定非零值，即可稳定重现。
  正确写入 `base+48`，旧 release 写入 `base+24`。
  旧机器码片段 `4a 8d 04 e3` 是 `lea rax,[rbx+r12*8]`，不是 ×16。
  另用系统 clang 的 arm64 汇编器与 otool 独立确认上述四个指令字，避免只依赖仓库自身
  解码器；记录见 `target/loopra-dbg/assembler.log`、`minimal-words.txt`。

### 敏感性实测

在添加回归矩阵后、应用翻译器修复前实际运行：

```sh
cargo test -p attesor q_register_offset_scale16_matches_interpreter -- --test-threads=1
cargo test --release -p attesor q_register_offset_scale16_matches_interpreter -- --test-threads=1
```

前者在 iced 内部报告 `left: 16, right: 8`；后者执行后报告
`memory option=2 scaled=true load=false` 不相等。
日志：`target/loopra-dbg/sensitivity-q.log`、`sensitivity-q-release.log`。
修复后的最初同种子重跑：`ok=10000 mismatch=0 skipped=0 loop_regions=9240`。
原始发现日志为 `discovery-q.log`、`discovery-q-off.log`、`fixed-10k.log`。
生成器随后继续增加覆盖，因此最终生成器的相同种子不再对应原 case 编号。

## 修复与正确性依据

- 只在有效移位大于 3 时，将 index 复制到 RCX，显式 `SHL RCX,4`，再使用
  合法的 ×1 LEA。原先可编码的标量缩放和不缩放形式保持原快路径。
- UXTW 仍先用 32 位 MOV 清除旧 Xrm 高半部；绝不在借用的区域寄存器上移位。
  因而不会破坏 guest index，且 Rm/Rn 别名时仍先保存 index 再读取 base。
- 显式移位在 64 位上得到 `index*16 mod 2^64`；LEA 加 base 后，原来的 TBI/
  commpage 处理不变。地址等于 ARM 的地址计算，且不改变任何 guest NZCV。
- `aot.rs::TRANSLATOR_VERSION` 从 50 升至 **51**，使旧错误 AOT 代码失效；
  新代码只用寄存器操作，不引入任何绝对主机地址。
  最小 LSL 程序的修复机器码含 `4c89e1 48c1e104 488d040b`，即
  `mov rcx,r12; shl rcx,4; lea rax,[rbx+rcx]`，见 `fixed-minimal-machine-code.txt`。

## 审查中记录的可疑点与核验

| 可疑位置 | 审查/测试结果 |
| --- | --- |
| `src/dst/copy/init_dst` 借用的 W 源带脏高半部；同寄存器复制被省略 | 读取 W 不能破坏 X 源；算术、比较用 32 位操作数，MOV 自赋值、identity bitfield/EXTR 和 select 结果保证 W 写零扩展。高半部非零、零寄存器及别名矩阵通过。 |
| `region_binary` 两地址覆盖 Rm，尤其 SUB；移位/取反借用 Rm | SUB 先保存 RHS；交换只用于可交换操作；原地移位/取反只允许 Rd=Rm 且 Rd!=Rn。随机强制 Rd=Rn/Rm、ZR、宽/窄压力通过。 |
| MADD/MSUB 的 addend 与 Rd 别名 | addend 先捕获至 RDX，再计算乘积；原有别名单测及扩展 loops 通过。 |
| `region_dp_imm` 的 CMP/TST、MOVK、W 逻辑立即数 | 比较不写源；MOVK 最终清高位；立即数/位域及脏高半部覆盖通过。 |
| 寄存器偏移的扩展与缩放 | **确认并修复 Q ×16 缺陷**。UXTW 脏高半部、负 SXTW、LSL、SXTX，以及目的=base/index 的保存恢复探针均纳入测试。 |
| LDRSB/LDRSH/LDRSW 目的宽度 | 区分 sign-extend-to-X 与 sign-extend-to-W 后清高半部；unsigned/unscaled/pre/post/register-offset 随机合法形式通过。 |
| pre/post-index、pair 地址保存和 base 与 load 目的别名 | 区域保存原地址到 RDX；assigned base 直接更新时排除重叠；pair 第二次访存不依赖已覆盖的 guest base。原有 commpage/pair 单测和新别名探针通过。 |
| dirty mask、helper 早退出、公共 successor/patchable stub | in-body barrier 同步所有槽；helper 后 MOV reload 保留返回状态；exit 写回 union dirty 集。原有 helper/pending/FPCR/链式 successor 单测通过。 |
| `loop_exit_flags` 延迟 NZCV，退出后首次读 flags | exit 上先物化 flags，再做 MOV 写回；有 call/commpage 时不做该延迟证明。loops 加入两个 CSET 立即读取退出 flags，另保留 side-exit/helper 测试。 |
| W CBZ/TBZ 错读 X 高半部 | 新增确定性两迭代探针：低半部 wrap-to-zero 与翻转 bit31，高半部始终非零；Plain/Patchable 均匹配解释器。 |
| R11 TBI mask 跨回边、helper、FP cold path | 回边在失效时用 MOV 恢复；已有 helper 故意清 R11 的 tagged-pointer 单测和 cold-register 写集合检查通过；loops 还使用 tagged base 和 pointer-shaped store payload。 |
| commpage 条件 helper、caller-saved 区域槽与栈临时值 | call 前同步；保存 RDX/R9 的两个 push/pop 保持 16 字节对齐；返回 reload 区域槽。既有故意破坏 RDX/RSI/RDI/R8/R9/R10 的 helper 单测通过。新 Q 矩阵还覆盖启用 commpage 检查的普通地址路径。 |
| Plain/AOT、私有入口、最大块长、反复短内层循环 | Q 矩阵同时执行 Plain/Patchable；既有 Plain 双地址重定位单测通过；生成器加入恰好 96 指令块及多次 1–3 次迭代的内层循环进入。 |

额外记录：`flags_safe_passthrough` 对部分访存家族采用宽编码匹配，保留/非法访存编码
在 flags-liveness 证明中的合法性边界值得另行审计；本次合法操作数差分没有将其证明为
另一个实际失败，未将其混入已确认的 Q 缩放修复，也不声称完成非法指令路径证明。

## 增强的测试入口

```sh
target/release/attesor --selftest-jit loops 100000 0x1234
bash tests/attesor/loopra-dbg-check.sh
```

种子可以是十进制或 `0x` 十六进制，禁止全零 xorshift 状态。失败打印种子、初始 X/NZCV、
指令、寄存器/内存/PC/退出差异并返回失败。摘要给出实际区域进入数、最大块数、短内层
循环进入数、地址别名探针、Q ×16 指令和负 SXTW 案例数。

确定性新增单测：

- `jit::emit::loopra_tests::q_register_offset_scale16_matches_interpreter`：64 组合，
  两种 tail、commpage 检查开关、四种 extend、缩放开关、load/store；比较全部 X/V、NZCV、
  PC、SVC 与整块内存，且要求无 fallback、无非法 SIB scale。
- `jit::emit::loopra_tests::w_zero_and_bit_branches_ignore_dirty_upper_halves`。

## 最终验证记录

已完整执行 `bash tests/attesor/loopra-dbg-check.sh`，退出状态为 **0**。

- `cargo build --release -p attesor`：通过。
- `cargo test --workspace -- --test-threads=1`：50 个 attesor 测试、9 个 core 测试通过。
- `cargo test --release --workspace -- --test-threads=1`：同样 50 + 9 通过。
- 两个模式下所有 13 类各生成 100,000 次，26 条摘要均 `mismatch=0`。
  非 loops 随机类保留既有非法/不支持输入的 skipped 统计，没有将 skipped 算成通过。
- 额外 30 个独立种子批次、每批 100,000 个有效 loops：全部通过。
  种子是 `0x1234`、`0xdeadbeef`、`0x100f5e1f7e57cafe` 各加 0–9。
- 最终这次检查累计 **3,200,000 个 loops，mismatch=0，skipped=0**；
  其中关闭 loopra 的 100,000 个区域执行数为 0；开启时累计实际 public region
  进入数 **2,880,045**。loops 生成覆盖累计：最大长度块 100,000 个、短内层进入
  1,802,526 次、地址别名探针 700,000 个、Q ×16 指令 94,344 条、负 SXTW 案例 800,000 个。
  后面这些是生成覆盖计数，不将其冒充每个类别都进入了区域优化的执行数。
- `bash -n tests/attesor/loopra-dbg-check.sh`、`git diff --check`：通过。

工作区测试实际行（debug/release 各出现一次）：

```text
test jit::emit::loopra_tests::q_register_offset_scale16_matches_interpreter ... ok
test jit::emit::loopra_tests::w_zero_and_bit_branches_ignore_dirty_upper_halves ... ok
test result: ok. 50 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

完整 26 条类别矩阵实际行：

```text
off=none jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
off=none jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
off=none jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
off=none jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
off=none jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
off=none jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
off=none jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
off=none jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
off=none jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
off=none jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=92586 seed=0x1234 max_blocks=3125 nested_entries=55920 aliases=21875 q_scale16=2948 negative_sxtw=25000
off=none jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
off=none jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
off=none jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
off=loopra jit selftest [blocks]: ok=8893 mismatch=0 skipped=91107 insns=30638 fallback_insns=1178
off=loopra jit selftest [fuse]: ok=63096 mismatch=0 skipped=36904 native=63096 pairs=63096 dead_flag_skips=0
off=loopra jit selftest [fuse2]: ok=32637 mismatch=0 skipped=67363 native=32637 pairs=32637 dead_flag_skips=34629
off=loopra jit selftest [selchain]: ok=36205 mismatch=0 skipped=63795 native=36205 dead_flag_skips=26730
off=loopra jit selftest [fpblocks]: ok=100000 mismatch=0 skipped=0 insns=650599 fallback_insns=0
off=loopra jit selftest [dpimm]: ok=70035 mismatch=0 skipped=29965 native=56675
off=loopra jit selftest [dpreg]: ok=31044 mismatch=0 skipped=68956 native=30899
off=loopra jit selftest [ldst]: ok=52288 mismatch=0 skipped=47712 native=43879
off=loopra jit selftest [branch]: ok=53072 mismatch=0 skipped=46928 native=53072
off=loopra jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=0 seed=0x1234 max_blocks=3125 nested_entries=55920 aliases=21875 q_scale16=2948 negative_sxtw=25000
off=loopra jit selftest [fpnan]: ok=100000 mismatch=0 skipped=0 native=100000
off=loopra jit selftest [tbiblocks]: ok=100000 mismatch=0 skipped=0 native=1400000 fallback_insns=200000
off=loopra jit selftest [calls]: ok=100000 mismatch=0 skipped=0 native=87500 callret=0 deep=12500
```

压力测试最后一行及脚本结果：

```text
off=none jit selftest [loops]: ok=100000 mismatch=0 skipped=0 native=87500 loop_regions=93219 seed=0x100f5e1f7e57cb07 max_blocks=3125 nested_entries=56060 aliases=21875 q_scale16=2932 negative_sxtw=25000
PASS: both optimization modes, all classes; 3000000 extra seeded loop cases
```

全部 56 条摘要保存在 `target/loopra-dbg/selftests.log`；逐批完整日志、构建日志和
两种 profile 的工作区测试日志都在 `target/loopra-dbg/`，没有放到用户缓存目录。

## 文件与提交

- 翻译修复：`crates/attesor/src/jit/emit.rs:3342`。
- 回归矩阵：同文件 `q_register_offset_scale16_matches_interpreter`、
  `w_zero_and_bit_branches_ignore_dirty_upper_halves`。
- 生成器：`crates/attesor/src/jit/selftest.rs::loop_insn/run_selftest_loops`；
  种子 CLI：`crates/attesor/src/main.rs`。
- AOT 身份：`crates/attesor/src/aot.rs`，版本 51。
- 可重跑入口：`tests/attesor/loopra-dbg-check.sh`（可执行）。

保持在 `loopra-dbg` 工作树，未推送、未提交。该工作树 `.git` 指向禁止触碰的
`~/attesor/.git/worktrees/attesor-dbg`，因此没有尝试写入该元数据；按简报的回退规则，
提交消息保存在 **`target/loopra-dbg-commit-message.txt`**。

这证明了已缩减指令的修复与上述有界差分覆盖，不是所有 guest 程序的形式化证明；
真实 TLS/AppKit 复测以及前述非法编码 liveness 边界仍未执行。

