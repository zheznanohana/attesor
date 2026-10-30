# G3 移植报告（2026-10-30）

## 结果

按 **#10 → #18 → #20 → #6** 移植指定的 `feature-NN-production.patch`。
四项前提仍成立，**没有跳过任何功能**。补丁针对旧基线，采用工作树手工移植；
没有使用会写索引的 `git apply --3way`，没有 stage、commit、reset 等 Git 写操作。
保留开始时已有的 wave-1 / G2 暂存内容，不回滚或覆盖其他功能。

开关均为 `ATTESOR_JIT_ON` 的精确逗号 token，默认关闭，精确 `ATTESOR_JIT_OFF` 优先；
不做子串或空白容错。`codegen_id` 原有 ON/OFF/DISABLE 原文哈希继续提供同一二进制的
生成码/AOT 身份隔离。`ibtc2` 有效模式另占 `flags_word` **bit 10**，没有覆盖原补丁
bit 6 对应的现有 AVX-512 能力位。全部功能完成后仅统一递增一次
`TRANSLATOR_VERSION`：**62 → 63**，没有采用四份旧补丁的 54/55。

## 逐项接入与检查

每项完成后均运行 `cargo build --release -p attesor` 和
`cargo test --release -p attesor -- --test-threads=1`；定向门禁另外覆盖
默认、ON、ON+OFF、非精确 token，以及与 tiered/fastenc/preciseinvalidate/slotreuse/jitcost 组合。

| 顺序 | 功能 | 移植重点 | 当时的全量 attesor L0 |
| --- | --- | --- | --- |
| 1 | #10 `bliccache` | 1024 槽线程私有 BLR 站点缓存、完整 site/target/epoch、正/负命中与 dispatcher pending | 118 通过 |
| 2 | #18 `ibtc2` | 32768 entries / 两路 RR、TLS 绑定、表 epoch、生产填表验证 | 119 通过 |
| 3 | #20 `rel32chain` | 发布前 E9、永久 guard、不可变 incoming、失效先撤销、入口 CPUID | 121 通过 |
| 4 | #6 `notbi` | 小域精确事实、完整访问地址/末字节证明、真实 scalar immediate mask 省略 | 123 通过 |

#18/#20/#6 还分别运行了各自 ON 模式的全量 L0，均通过。
每项的自测：#10/#18/#20 为 `branch`、`calls` 各 1000 次抽样；
#6 为 `tbiblocks`、`blocks`、`loops` 各 1000 次抽样，均 `mismatch=0`。
随机编码自测会拒绝不适用指令：branch 有效 562 / 跳过 438，blocks 有效 103 / 跳过 897；
这些不是 1000 个全部执行的样例。calls、tbiblocks、loops 各完成 1000 例。

### #10 的集成差异

- Cpu 指针追加在既有 jitcost 尾字段之后，所有偏移由 `offset_of!` 获取。
- dispatcher pending 在冷解释之前消耗，接入现有 tiered/background 选择路径。
- 正缓存额外排除 tiered 冷入口，防止绕过 dispatcher 的完成计数/晋升。
- AOT 仍无等价 UnitRec 生命周期，保持负命中/原 IBTC 回退；未实现原补丁也未提供的 `callinline`。
- 定向执行覆盖 BLR、x30 alias、既有认证调用译码、碰撞/完整 tag、epoch、实际 SMC 后重译到 42，
  以及未跟踪/冷目标拒绝。callret 与 verify 保持旧调用路径。

### #18 的集成差异

- 生成路径保留 quick 层退出和 jitcost probe，默认仍为原一路布局。
- 两路填表持 TRANSLATE 时直接写本线程表，避免再次进入 tiered 的锁而死锁；同时排除冷入口。
- 调整调用/返回自测的私有 fixture 填表，改用实际布局的 `IndirectCache::insert`，不再写死一路索引。
- 定向执行覆盖两 way 命中、同 tag 更新/第三 tag 替换、PC 0/空槽/不对齐/高位 tag、
  跨线程 epoch、两 way 清除、旧 host 填表拒绝、SMC、AOT/冷目标拒绝和 TLS 重绑定。

### #20 的集成差异和发现的运行时错误

- 原补丁给每个 outgoing jump 再设置 label；只有一条 B 的块已有待绑定的入口 label，
  实际组装会报 `Only one label per instruction is allowed`。已用**指令序号 +
  new_instruction_offsets + slotreuse prefix**替代，默认生成路径不新增指令。
- 保留现有 source_ranges、精确失效、AOT 竞争重查和统计；旧冷层晋升也撤销 guard。
- 冷源不生成 direct 链，冷目标不能被 direct 链绕过；无 guard 的后台产物/AOT 不是 direct 目标。
- 将 CPUID 放到统一 Rust JIT 入口 wrapper，覆盖 dispatcher、verify 和直接自测入口。
- 全局禁止该模式的后发布 slot 链接；相应旧测试断言改为检查 dispatcher continuation，未降低其他模式断言。
- 定向测试检查实际 E9/disp32/NOP、两个有符号边界及越界/形状回退、跨线程发布、
  失效后源代码字节不变、dead guard 拒绝、运行中跨线程改变 epoch 后 still-alive guard 拒绝，
  最后重新发布并执行到新值 42。

### #6 的集成差异

- 接到现有访问 emitter 和新的 multi-loop 入口初始化；开启时每个公共/私有块入口都清空事实。
  默认关闭时不运行逐指令事实推进或清零，避免把 proof 扫描成本放入默认翻译路径。
- 测试检查真正省去的机器码 mask，并实际运行 ADR / ADD / scalar LDR / STR / 负偏移 LDUR，
  对比解释器寄存器、PC、NZCV 和内存；重定向 commpage 保留原 mask。
- 另测入口、helper、未知/partial/writeback/label 屏障、load/base alias、tagged base，
  以及 MOVZ W/X、ADR/ADRP 正负偏移、W 截断/溢出、完整访问末字节跨 bit 56 等边界。
- `ATTESOR_JIT_OFF=tbimask` 的旧 shifts 组合也通过。

## 编译限定补丁的敏感性证明

四份原始报告均只有编译证据；本次全部新增实际定向测试。
`tests/attesor/port-g3-check.py all --sensitivity` 逐一临时修改生产源、要求测试失败、
在 `finally` 恢复源码，再要求相同测试通过。四个变异体最终均以断言失败被检出，
不是把编译失败或零测试当作敏感性证明：

| 功能 | 受控故障 | 观察到的失败 |
| --- | --- | --- |
| #10 | 删掉完整 site 比较后的 miss 分支 | 碰撞站点错误执行 callee 得 7，而非缓存 miss / 99 |
| #18 | 第二 way 命中改为 miss | 得 99/CONTINUE，而非第二 callee 的 8/SVC |
| #20 | 绕过 revoked alive 检查 | 失效目标仍执行旧 7，而非无副作用返回 dispatcher |
| #6 | 地址证明忽略访问末字节 | `base=0xfffffffffffff9` 的 8 字节访问被错误证明为安全 |

运行 fixture 也显式演示未失效的旧代码返回 7（或 8），与新 guest 指令的 42 不同。
没有遗留上述源变异。

## 最终验收

- 最终 `cargo build --release -p attesor`：通过（既有 warning 保留，未扩散修复）。
- 最终 `cargo test --release --workspace -- --test-threads=1`：attesor **123**、core **9** 通过；difftest 无单测。
- 四项同开 + tiered/bgtrans/fastenc/preciseinvalidate/slotreuse/jitcost 的全量 attesor L0：**123 通过**。
- 四项同开、再加 callret、全部 OFF、bliccache verify 和 notbi 旧 shifts 的定向门禁：通过。
- 四项同开：tbiblocks、blocks、fpblocks、loops、calls、jitcode 各 1000 次抽样，全部 `mismatch=0`。
- 四项 + slotreuse/fastenc：jitcode **1000 通过**，八种 host residue 均覆盖，计数
  `[115, 194, 107, 36, 89, 117, 133, 209]`。
- 四项 + callret：calls **1000 通过**，`mismatch=0`。
- 四项同开，实际磁盘 AOT `offline` 自测：**128 通过，833 disk_loaded_blocks，mismatch=0**。
- 同一测试二进制的 default、四个单独 token、全 ON、ON+OFF 共 **7 个不同 AOT/codegen identity**；有效 ibtc2 bit 10 断言通过。
- Python 语法检查和 `git diff --check`：通过。

可复跑入口：`python3 tests/attesor/port-g3-check.py all --sensitivity`。
完整日志：`target/port-g3/logs/`（各阶段 `*-L0*.log`、`final-gates.log`、
`final-gates-after-off-gating.log`、`6-final-sensitivity.log`、
`final-build.log`、`final-workspace-L0.log`、`final-offline.log` 等）。
提交说明文本：`target/port-g3-commit-message.txt`，仅写文件，未创建提交。

## 保留的边界

这不是性能验收，也不是无协作并发 SMC、信号重入、代码回收或全 AOT 生命周期的完整证明。
保留 append-only code cache、线程私有表和现有 IC/ISB/dispatcher 同步合约；
没有任意已发布指令热补丁、AOT direct rel32 或后来目标的热回填。
缓存查询、guard、CPUID 和禁止热 slot 链接可能回退性能，未作收益声明。
