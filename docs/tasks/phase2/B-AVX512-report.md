# Track B — Round 2 报告（2026-10-29）

## 结论与检测边界

已修正 macOS lazy AVX-512 检测，补做真实高寄存器执行、敏感性证明、NEON/磁盘 AOT 差分和三轮交替测量，并实现独立 opt-in 的 NaN/range 阶段。**仍是候选优化，不默认开启，未宣称性能验收通过。**

本轮独立 128 位 EVEX 探针实测 `XCR0 before: 0x7` → `XCR0 after: 0xe7; correct=1`，推翻旧报告“XCR0=7 即不能执行”的结论。然而本工具环境的四个 `sysctlbyname` 仍全部返回 `EPERM`；不能把查询失败当作 OS 授权。生产二进制因此按保守策略返回 `has_avx512() == false`，即使 ON 也走回退。

为不再将回退误算作原生验证，新增 `tests/attesor/avx512-execution-fixture.py`：先编译并实际执行 CPUID 门控的 128 位 EVEX 探针；仅在探针正确退出后，复制源码到本工作树 `target/`，**只在副本中替换 OS 检测结果**，构建测试二进制。生产源码、用户安装和缓存均不变。副本保留所有实际 translator 实现和 ON/OFF 开关；源码 SHA256、探针和 uptime 写入 `fixture.json`。这些测试/计时是实际原生路径，但不等同于未替换检测的生产端到端验收；禁止安装该副本或复用其缓存。

## 修改

### Step 1（已先冻结为独立补丁/提交消息）

- V 缓存 8 → 24 槽，xmm8–xmm31；高寄存器传输、标量读取、二元 RHS、脏驱逐和冷路径使用 128 位 EVEX。编码和执行覆盖全部 xmm16–xmm31，保持原八槽回退。
- macOS 用 CPUID AVX512F/VL/BW/DQ **且**四项 `hw.optional.avx512{f,vl,bw,dq}` sysctl；查询失败、长度不符或值不为 1 均关闭。非 macOS 保留 Rust 的硬件/OS 检测并要求全部四项。新增逐项缺失和 lazy-XCR0 策略单测。
- `ATTESOR_JIT_ON=avx512` 明确 opt-in，OFF 优先；AOT bit 6/7 记录能力/选择，translator version 53 → 54。
- 第一阶段快照在 `target/avx512/round2/snapshot-step1`；后续补丁以此为基准，不混入 NaN 阶段。

### Step 2（NaN/range）

- 独立 `ATTESOR_JIT_ON=avx512nan`，可与 `avx512` 组合，OFF 优先，同样受生产能力检测约束。
- 精确匹配 x86 negative-indefinite 的整数比较写 k1，再以合并掩码写 ARM default-NaN；不覆盖其他 NaN 载荷。
- VFPCLASS + 掩码 VFIXUPIMM 进行 quieting，保留符号/载荷，仍按 ARM 的反向 qNaN pass、再 sNaN pass 处理优先级；FMA 的 inf×0/quiet-addend 特例保留。
- VRANGE 的 min/max + selected-sign 处理 NM 形式、相反符号零和 DAZ/FZ；传播式 max/min 另走 ARM NaN 覆盖冷路径。
- 冷路径 ABI 增加仅 k1 scratch，禁止写其他 k 寄存器或高 XMM 缓存。新编码测试检查无 YMM/ZMM 和冷寄存器契约。
- translator version 54 → 55；AOT bit 8 记录 NaN/range 选择，分别翻转 bit 6/7/8 验证拒绝不兼容缓存。
- 增加真实 EVEX 探针、隔离执行 fixture、28 值压力 guest、实际指令出现断言、可选 L0 类别/flags 和基准 variants。

指令语义依据 [Intel SDM Vol. 2C](https://www.intel.com/content/dam/www/public/us/en/documents/manuals/64-ia-32-architectures-software-developer-vol-2c-manual.pdf) 的 VFIXUPIMM、VFPCLASS、VRANGE 条目；不是直接把 x86 MIN/MAX 或全部 NaN 归一化当作 ARM 等价实现。

## 测试命令与实际结果

所有缓存均在本工作树 `target/` 或脚本私有临时目录；没有 GUI、用户缓存、安装或 push。构建保留现有 warnings，未扩大处理其他轨道代码。

```sh
TMPDIR=/private/tmp ATTESOR_CACHE="$PWD/target/avx512/round2/production-cache" cargo build --release --offline
TMPDIR=/private/tmp ATTESOR_TEST_AVX512=1 ATTESOR_CACHE="$PWD/target/avx512/round2/unit-final-cache" cargo test --release --workspace --offline -- --test-threads=1 --nocapture
python3 tests/attesor/avx512-execution-fixture.py target/avx512/round2/fixture-step1 --step1-snapshot target/avx512/round2/snapshot-step1
python3 tests/attesor/avx512-execution-fixture.py target/avx512/round2/fixture-nan
```

`ATTESOR_TEST_AVX512=1` 只存在于 `cfg(test)` 授权和线程局部测试选择中，用于已先成功执行探针的本机；它不能开启生产二进制的 AVX-512。

- Step 1 L0：release build；attesor **72 passed**、core **9 passed**；五个受影响类各 20,000 次，mismatch=0。
- 最终 L0：release build；attesor **75 passed**、core **9 passed**，0 failed。
- 真实高寄存器差分：3 seeds（1、0x12345678、0xfeedbeef）、32 种精度/操作/别名/stub 形式、每种 8×8 输入，共 6144 个正确高-bank case，全部与解释器一致；回退亦一致。故意将 xmm31 缓存复制来源改为 xmm5，每组 192 case 中 **117–192** 个 mismatch；32 组共 **5016** 个，全部要求 >0。
- NaN/range 定向差分 **340256 case，mismatch=0**，含 FZ 开/关、混合向量 lane、正负零、denormal、正负 inf、sNaN/qNaN、负 indefinite、FMA addend 和 NaN 优先级。检查所有 V、X、NZCV、FPCR；不把它描述为完整 FPSR 异常语义证明。
- VFIXUPIMM mutant 将 NaN 原始源改成当前结果，丢失载荷/优先级；VRANGE mutant 翻转 min/max。每个适用模板均产生 >0 差异，原始数字见 JSON 和 `nan-directed.log`/`unit-final.log`。
- `cfg(test)` 线程局部配置 `(cache,nan)=(true,false)/(false,true)/(true,true)` 实际运行五类×20,000，各输出 mismatch=0。不是生产 detector 回退的“ON 测试”。

五类的最终原生测试行（各配置一致；seed=1，部分原有类使用自己的固定种子）：

```text
jit selftest [fpblocks]: ok=20000 mismatch=0 skipped=0 insns=129744 fallback_insns=0
jit selftest [fpnan]: ok=20000 mismatch=0 skipped=0 native=20000
jit selftest [0E20D400:0E20D400]: ok=4377 mismatch=0 skipped=15623 native=1288
jit selftest [0F800000:0F800000]: ok=2262 mismatch=0 skipped=17738 native=273
jit selftest [1F000000:1F000000]: ok=3197 mismatch=0 skipped=16803 native=1200
```

隔离 fixture 上的完整可执行工作负载命令：

```sh
python3 tests/attesor/avx512-check.py target/avx512/round2/fixture-step1/build/release/attesor --count 20000 --classes fpblocks fpnan 0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000 --output target/avx512/round2/check-native-step1
python3 tests/attesor/avx512-check.py target/avx512/round2/fixture-nan/build/release/attesor --count 20000 --classes fpblocks fpnan 0E20D400:0E20D400 0F800000:0F800000 1F000000:1F000000 --flags '' avx512nan avx512,avx512nan --output target/avx512/round2/check-native-nan
# 新增压力/AOT 和实际原生指令断言；step1 同样运行 --flags avx512：
python3 tests/attesor/avx512-check.py target/avx512/round2/fixture-nan/build/release/attesor --directed-only --require-native --flags avx512nan avx512,avx512nan --output target/avx512/round2/directed-native-nan
```

- OFF、cache、nan、组合配置均通过上述五类；每种配置的 `loopra-aot-check.sh` **7/7**：native=interp=cold-JIT=warm-AOT，并验证 loopra 缓存身份。
- `neon_edge.c`：解释器 = OFF JIT = cache JIT = nan JIT = 组合 JIT。
- 28 值压力 guest：所有四种配置 **解释器 = cold-JIT = warm-main-AOT**，校验和 `pressure a442eaf722d4a374`。warm 只保留 main AOT，日志确认加载 pressure 的磁盘块；输入包含 NaN/inf/zero，全部输出 lane 参与校验。
- `--require-native` 保存并反汇编压力 `--jit-dump`，实际断言存在 xmm16–31 和/或 VFIXUPIMM，防止回退冒充原生。组合反汇编可见 `vmovdqa64 %xmm0,%xmm31`、`vmulsd %xmm31,...`、`vfixupimmpd ... {%k1}`。
- Python 三脚本 `py_compile`、shell `bash -n`、`git diff --check` 通过；分步补丁顺序应用校验通过，结果与交付文件逐字节一致。

## 测量

下面均为**明确替换 OS 检测的原生测试副本**。每轮 OFF → ON 交替至少三次，单位 ms；计时含 guest 自报内核时间，校验和始终 `5276637 / 2694 / 33848033760`。系统负载波动大，不作吞吐提升或频率结论。生产二进制的 OFF/ON 回退原始测量另外保留在 JSON 的 `measure-step1`，不计为 AVX-512 收益。


### Step 1：八槽 vs 24 槽

|轮次|ON|fp-nbody|matmul|neon-dot|uptime|
|---|---|---:|---:|---:|---|
|1|OFF|119.694|86.244|7.525|22:27  2 users, load averages: 6.56 6.16 8.54|
|1|avx512|117.900|94.157|8.567|22:27  2 users, load averages: 6.56 6.16 8.54|
|2|OFF|152.993|119.482|12.536|22:27  2 users, load averages: 6.56 6.16 8.54|
|2|avx512|203.748|173.246|15.652|22:27  2 users, load averages: 6.56 6.16 8.54|
|3|OFF|243.826|161.675|15.369|22:27  2 users, load averages: 6.56 6.16 8.54|
|3|avx512|194.905|121.520|8.844|22:27  2 users, load averages: 6.56 6.16 8.54|

`avx512` 配对 ON/OFF 中位比（依次三内核）：0.9850, 1.0918, 1.1385；不作为接受结论。

### Step 2：原路径 vs NaN/range vs 组合

|轮次|ON|fp-nbody|matmul|neon-dot|uptime|
|---|---|---:|---:|---:|---|
|1|OFF|124.275|88.606|8.084|22:29  2 users, load averages: 7.65 6.66 8.51|
|1|avx512nan|111.886|87.006|7.391|22:29  2 users, load averages: 7.65 6.66 8.51|
|1|avx512,avx512nan|115.659|90.930|8.358|22:29  2 users, load averages: 7.65 6.66 8.51|
|2|OFF|113.331|86.724|7.494|22:29  2 users, load averages: 7.65 6.66 8.51|
|2|avx512nan|112.112|86.739|7.453|22:29  2 users, load averages: 7.65 6.66 8.51|
|2|avx512,avx512nan|113.019|87.128|8.115|22:29  2 users, load averages: 7.65 6.66 8.51|
|3|OFF|120.507|91.269|7.962|22:29  2 users, load averages: 7.65 6.66 8.51|
|3|avx512nan|119.424|84.263|7.221|22:29  2 users, load averages: 7.65 6.66 8.51|
|3|avx512,avx512nan|114.094|86.882|8.130|22:29  2 users, load averages: 7.65 6.66 8.51|

`avx512,avx512nan` 配对 ON/OFF 中位比（依次三内核）：0.9468, 1.0047, 1.0339；不作为接受结论。

`avx512nan` 配对 ON/OFF 中位比（依次三内核）：0.9892, 0.9819, 0.9143；不作为接受结论。

### 指令计数（更可靠的结构证据）

从同一 arm64 基准抽取实际内层块，`ATTESOR_DUMP_BYTES=16384 --jit-dump` 后反汇编，计数沿默认 FPCR 分支的线性热路径直到首个无条件转移，含入口/退出，不含冷尾部或 dispatcher；不是动态执行总数。两个原生 fixture 各配置均相同：

|内核|guest 指令|OFF|cache|nan|组合|
|---|---:|---:|---:|---:|---:|
|fp-nbody|23|163|163|163|163|
|matmul|7|39|39|39|39|
|neon-dot|9|47|47|47|47|

这些内层热块没有得到减少：扩大缓存不等于所有工作负载受益，整数 NEON dot 本身也不调用 NaN 修复。相比之下，定向模板的 **cold-range 静态指令数（含恢复 jump）**，单/双精度一致：

|模板|原路径|nan|
|---|---:|---:|
|FADD|52|13|
|FMAX|69|9|
|FMAXNM|63|0|
|FMA|93|37|

FMAXNM 的冷尾部消失是被热路径单条 VRANGE 替代，不是省略语义。上述结构收益不能外推到普通有限数密集应用；传播式标量 min/max 热 guard 也有开销权衡。

## 开放风险 / 未执行项

1. 生产检测的 CPUID+sysctl 正路径仍需在允许查询的真实运行上下文验收；当前实际返回 false。test fixture 是公开标注的 detector substitution，不是生产默认开启的依据。
2. 本轮满足 L0，并补做 loopra/AOT 和 headless C 工作负载；**没有重跑完整 L1 全类×100000、gdbstub、headless TLS，也没有 L2 多 seed×300k、GUI/真实应用验收**。上轮 TLS 为 ENOTFOUND，本轮未宣称联网通过。合并前由 Claude 补 L1，default-on 前补 L2。
3. 定向 NaN/FZ 测试主要证明数据结果和寄存器状态，未穷尽 FPSR 异常位、全部 rounding mode。DN/AHP/FZ16 的现有 interpreter guard 未放宽。
4. 测量存在较大负载噪声；所选内核热块指令数未降，不能宣称全局加速或 AVX-512 频率行为已证实。
5. 改动保持 opt-in；需要 Claude 审核接受后才能讨论默认开启。

## 交付 / 提交消息与补丁

Git 管理目录指向工作树之外的 `/Users/zhez/attesor/.git/worktrees/attesor-p2b`，不在可写范围，且 COMMON 禁止触碰该树；因此**未创建 git commit，未 push**。依 COMMON 采用提交消息文件与分步补丁。应用顺序：

1. `target/avx512-step1.patch`（同 `target/avx512-round2-step1.patch`）；消息 `target/avx512-step1-commit-message.txt`。
2. `target/avx512-nan.patch`；消息 `target/avx512-nan-commit-message.txt`。
3. `target/avx512-round2-report.patch`；消息 `target/avx512-round2-report-commit-message.txt`。

当前报告替代上轮候选报告；上轮原始数据仍在 `docs/measurements/2026-10-29-avx512.json`，不把其“ON 回退”重新解释为原生结果。本轮持久证据：[2026-10-29-avx512-round2.json](../../measurements/2026-10-29-avx512-round2.json)；完整构建/测试/原生反汇编/fixture/测量在 `target/avx512/round2/`。
