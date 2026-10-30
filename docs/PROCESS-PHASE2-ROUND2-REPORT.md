# Track P Round 2：native fixups 与 no-exec spawn 审查

2026-10-29（JST），`process` / `/Users/USER/attesor-p2p`，基点 `70c4f28`。
顺序：#30 原生 fixup 实验与验证，然后 #29 可行性报告。**#30 正确性对照通过，但没有
通过性能验收，默认关闭；#29 审查完成，当前 NO-GO，没有实现。** #28 留给 wave 2；
Round 1 的 process-qos 实验未改动，没有将其变成默认行为。

## 1. 实际改动

- `nativefixups.rs`：`ATTESOR_JIT_ON=nativefixups`；逗号分隔精确 token，
  `ATTESOR_JIT_OFF=nativefixups` 优先。所有常规运行默认不安装 hook。
- 对本机 guest dyld `57f4e7e7-a497-38bb-a3d2-a459c622460b`，同时验证 local symbol、
  原始函数完整字节 hash（`0xbb1b74bf045843f7`，652 bytes）和 PACIBSP，才在**本进程
  COW 映射**的 `dyld+0x4dfbc` 入口写入专用 SVC。没有修改 `/usr/lib/dyld` 文件。
  未知 dyld UUID、symbol 或代码不匹配则不安装，不推测私有 ABI。
- 实际入口是 `dyld_map_with_linking_np` 的**用户态 eager fallback**。现有 runtime 对
  syscall 550 返回 ENOTSUP；dyld 随后进入它。该符号仍带旧四参数 mangling，但本机
  反汇编实际调用 `(mwl_info_hdr*, blob_size)`，不是按旧声明猜测参数。
  它是 `JustInTimeLoader::applyFixups → applyFixupsGeneric → setUpPageInLinking`
  的后代，保留上层所有工作。
- Rust 解析 bounded mwl v7、segment chain starts、**dyld 已解析及 interpose 后的 bind
  target 表**，支持 `DYLD_CHAINED_PTR_64 (2)` 与 `64_OFFSET (6)`。重基、high8、无符号
  8-bit embedded bind addend 均按该 dyld 的 fallback 运算处理。
  没有自行实现符号查找、weak lookup 或 interposition。
- 在任何写入前完成整个调用的格式、边界、ordinal、overlap、guest 非代码可写 segment、
  实际 host VM R/W protection 检查。unsupported/multi-start/malformed/read-only 情况
  跳过已替换的 PACIBSP（现有 emulator 的 PAC identity 语义），继续原始 guest 函数。
  不放宽数据页权限；原生 writer 只写 fixup slots，绝不把旧快照覆盖到其他数据字节。
- `engine.rs` 只在上述准确 PC 或私有 return trampoline 接受专用 SVC；使用现有 interpreter/
  JIT exit 接口。没有修改 `jit/`、`emit.rs` 或 core 指令语义。
- `main.rs` 在执行 / 注册 AOT 之前安装；`aot.rs` 的 offline prewarm 对私有 text mapping
  做同样的 patch，避免 ON prewarm 生成未拦截的旧函数。输入指令改变会改变生成代码，
  因此按 COMMON 将 `TRANSLATOR_VERSION` **53 → 54**；现有 JIT flag/build 身份隔离保留。
- `ATTESOR_NATIVEFIXUPS_VERIFY=1`（须同时 ON）：先保存 encoded slots，执行**实际原生
  writer**，全页比较计划结果；只恢复 encoded slots，运行原始 guest dyld；在返回时再次
  全页逐字节比较。使用同一进程、同一原始输入和 bind 表，没有 ASLR 归一化、抽样或
  仅 hash 比较。每个含 fixup 的完整 chain page（这里为 16 KB）都比较，包含未修改字节。
- `tests/attesor/nativefixups-check.py`：只运行 private CLI、Foundation console 和 Electron
  的 headless Node 模式；真实 AOT、interpreter、6 children、unknown dyld 回退与交替 A/B。

## 2. 先纠正 profile 归因，再评价性能

Round 1 的 78/328（23.78%）execution samples 属于
`JustInTimeLoader::applyFixups..._block_invoke.31`。本机反汇编 `dyld+0x417b0`
显示它调用 `resolvedAddress`、`interpose`，然后向 bind target array 追加结果；
**它不是遍历 encoded pointers 的 fixup 写入循环**。因此不能把 24% 当作这次可消除的成本。

本轮独立 OFF Node profile：该 callback 91/672（13.54%）执行样本；
`dyld_map_with_linking_np` 13/672（1.93%）。这些是 sampling attribution，
不是总启动 CPU 百分比，也不是完整的 inclusive function timing。
原始数据与报告：`target/p-round2/accepted-check/profile-{off,on}/`、
`target/p-round2/accepted-profile-{off,on}.txt`；反汇编保存在 `dyld-disassembly.txt`。
参考上层设计与 format 语义时查阅了
[Apple dyld Loader](https://github.com/apple-oss-distributions/dyld/blob/main/dyld/Loader.cpp)、
[JustInTimeLoader](https://github.com/apple-oss-distributions/dyld/blob/main/dyld/JustInTimeLoader.cpp)、
[chain formats](https://github.com/apple-oss-distributions/dyld/blob/main/include/mach-o/fixup-chains.h)；
**安装依据是本机字节与符号，而非假定 main 分支源码与本机二进制相同。**

Node Electron Framework 这一次调用含 652,675 rebases / 8,194 binds：按位置数 98.76%
是 rebases。原生计划+写入耗时 **54.713 ms**，同一 encoded input 的 guest dyld loop
耗时 **7.474 ms**（经过时间，不是隔离的 CPU 时间；验证页复制/比较/恢复不计入这两个
数字）。所以即使写入位置以 rebase 为主，当前预检/快照式实现的总成本仍比 guest loop 大。
符号解析照常由 dyld 执行；不能将其 callback 样本算作本优化收益。

### 普通 launch-to-exit：三组交替 OFF / ON

私有 DSC **已 warm**，同一 release binary，两边 `--no-aot`，exit/checkpoint builder
关闭；两边开启 STATS，ON 还输出 fixup 诊断。CPU 为被等待 child 的累计 CPU，
不是 translator CPU；wall 包含完整启动、guest 程序与退出，不是 posix_spawn syscall latency。
Node 工作负载是打印固定 SHA256 的 headless crypto CLI，**不是 `node -e 0`**；
trivial 不触发此 fallback，是负控制。验证模式数据不混入 A/B。

| workload | OFF wall s（全部三次） | ON wall s（全部三次） | ON/OFF 均值 |
|---|---|---|---|
| trivial arm64 | 0.432074, 0.268911, 0.231177 | 0.489372, 0.252093, 0.226222 | 1.038 |
| Node crypto CLI | 2.402621, 2.082429, 2.063001 | 2.179627, 2.112812, 2.031171 | 0.966 |
| Foundation console | 0.426352, 0.378431, 0.438841 | 0.434424, 0.365119, 0.428078 | 0.987 |
| big dylib / reopen | 0.345416, 0.301124, 0.308767 | 0.303583, 0.270915, 0.261444 | 0.875 |

| workload | OFF CPU s（三次） | ON CPU s（三次） | ON/OFF 均值 |
|---|---|---|---|
| trivial | 0.273370, 0.260262, 0.225816 | 0.274741, 0.246755, 0.221205 | 0.978 |
| Node | 2.419640, 2.111295, 2.109562 | 2.220590, 2.148954, 2.083277 | 0.972 |
| Foundation | 0.417932, 0.369213, 0.429208 | 0.425998, 0.359500, 0.420388 | 0.991 |
| big dylib | 0.334781, 0.295059, 0.300959 | 0.296609, 0.265508, 0.254660 | 0.878 |

`uptime` 起始 `22:56 2 users, load averages: 17.40 17.03 13.86`，
最后 `22:57 2 users, load averages: 12.17 15.71 13.55`。逐次 uptime 全部在原始 JSON。
Host macOS 26.7.2 / 25G332，x86_64；sysroot `26.6.2__Mac14,2`。

**不能从这批噪声中的 ratio < 1 宣称提速。** 此前完整复现 `final/` 与 `final2/` 也保留，
例如前一批 Foundation ratio=1.083；机器 load 高且每组有时间趋势。更直接的同输入
loop 对照显示 native 更慢。当前实验只完成正确性可复现，不接受为启动优化、不默认开启。

可随补丁保存的原始记录：
`docs/measurements/2026-10-29-process-round2.json`（48 次运行，包含 24 次 A/B、逐调用 fixup
counts/timing、全部 uptime、binary/source SHA256）。完整 stdout/stderr、profiles、private
DSC/AOT 仍在 `target/p-round2/accepted-check/`。目录名仅表示这批 QA 通过，**不表示性能接受**。

## 3. 实际测试行

```text
TMPDIR=/private/tmp cargo build --release
  PASS（现有 warnings；没有为此修改其他 track）
TMPDIR=/private/tmp cargo test --release --workspace -- --test-threads=1
ATTESOR_JIT_ON=nativefixups TMPDIR=/private/tmp cargo test --release --workspace -- --test-threads=1
  两组各 attesor 75 passed / core 9 passed / 0 failed
  nativefixups directed unit tests: 7 passed
python3 tests/attesor/nativefixups-check.py --output target/p-round2/accepted-check
  PASS trivial: OFF=shadow dyld=actual native=OFF precedence
  PASS node: OFF=shadow dyld=actual native=OFF precedence
  PASS foundation: OFF=shadow dyld=actual native=OFF precedence
  PASS big: OFF=shadow dyld=actual native=OFF precedence
  PASS interpreter big dylib; ON prewarm + two actual AOT launches
  PASS six children: env/argv/IPC/PID/exit; unknown-UUID private dyld runs unhooked
  PASS 3 alternating A/B pairs per workload; 48 runs returncode=0
```

实际逐页比较范围（pages 为完整 16 KB chain pages 换算的 4 KB 数，含未改动字节；
多次调用/reopen 的页计数不去重）：

| workload | verified calls | 4 KB-equivalent pages | rebases | binds |
|---|---:|---:|---:|---:|
| Node | 8 | 1,984 | 664,267 | 9,977 |
| Foundation | 2 | 8 | 21 | 16 |
| big dylib | 3 | 524 | 131,072 | 131,081 |

大型独立 dylib 有 131,072 个表项，覆盖本地 rebase、已解析 bind、positive addend、missing
weak import，并两次 dlopen/dlclose/reopen。真实 AOT 日志确认加载 29 main blocks 与
30,412 dyld blocks，仍实际进入 native 路径，不仅检查了 prewarm 命令退出。

额外敏感性证明：故意在每个 native write run 后翻转一个结果字节，native unit tests
**2 failed / 5 passed，exit=101**；全页比较与指针值检查都捕获错误。立即恢复后重跑完整
OFF/ON workspace tests 和集成矩阵，均通过。日志 `target/p-round2/sensitivity.log`。
开发中发现并修正了 16 KB snapshot 却只读取 4 KB 比较的 verifier 错误，新增越过第一个
4 KB 的非 pointer 字节变异测试；最初失败日志 `integration2.log` 保留。初版 Mach write
逐段写入很慢，改为完成 VM 预检后仅 memcpy fixup slots，旧测量 `check3/` 仍保留。

这是 native/process 子系统，按 COMMON 使用 directed tests 与真实 headless workloads，
不跑 translator 随机矩阵。**L1/L2 尚未执行**（全 classes、多 seed、loopra/gdb/TLS/GUI
均未为本轮重跑）；没有将 Round 1 的 TLS DNS 失败冒称为本轮成功。
`rustfmt` 本机未安装，未安装新 toolchain 或修改环境；`git diff --check` 通过。

## 4. #29 可行性与未解决风险

完整报告：[PROCESS-SPAWN-WITHOUT-EXEC-FEASIBILITY.md](PROCESS-SPAWN-WITHOUT-EXEC-FEASIBILITY.md)。
父进程自 fork 消除了 server 的 parent/waitpid 代理问题，但当前缺少：全局多线程 fork
协议、guest VM 所有权与彻底清理、mutable shared-cache data 复原、JIT/AOT generation
reset、完整 Darwin spawn actions/attrs、CLOEXEC/signal/port/error-channel 状态转换。
**只审查，不实现、不发布 no-exec 开关、不宣称节省 450 ms 或测到了 no-exec spawn latency。**

#30 自身限制：仅这个已审计 dyld、mwl v7、formats 2/6、单 chain start；其他 dyld/格式回到
原 guest；禁用 page-in linking 后走另一条 generic loop 时也不会加速。Foundation 等缓存内
映像仍沿用原 shared-cache slide 路径。原生 stores 与原 guest dyld 一样假定 loader lock
保护下目标 image 不会被另一线程并发 unmap/mprotect；没有提供防任意 guest VM 竞态的
事务式内存模型。验证模式重复执行 write/restore 仅用于受控 headless 测试，不推荐应用使用。
当前快照、dirty bitmap、解析及 VM 检查成本较高，**性能失败是保留默认关闭的直接理由**。
没有 closure persistence、live shared translations 或 zygote 实现。

## 5. 提交材料与重现

Git metadata 指向 `/Users/USER/attesor/.git/worktrees/attesor-p2p`，不在允许写入区域，也受
COMMON 的主树禁写规则约束；没有尝试绕过权限、提交或 push。按逻辑步骤保存：

1. 提交说明 `target/p-round2-nativefixups-commit-message.txt`；
   patch `target/p-round2-nativefixups.patch`（runtime、tests、本报告、原始 JSON）。
2. 提交说明 `target/p-round2-spawn-feasibility-commit-message.txt`；
   patch `target/p-round2-spawn-feasibility.patch`（#29 可行性报告）。

```sh
TMPDIR=/private/tmp cargo build --release
TMPDIR=/private/tmp cargo test --release --workspace -- --test-threads=1
ATTESOR_JIT_ON=nativefixups TMPDIR=/private/tmp cargo test --release --workspace -- --test-threads=1
python3 tests/attesor/nativefixups-check.py --output target/p-round2-repro
python3 tools/profile-report.py target/p-round2-repro/profile-off
```

所有写入的测量、CLI guests、修改过 UUID 的 dyld **副本**与缓存都在本工作树；没有启动
GUI、改用户应用、使用用户 Attesor 缓存、改 `~/bin`/launcher 或操作非本次启动的进程。
