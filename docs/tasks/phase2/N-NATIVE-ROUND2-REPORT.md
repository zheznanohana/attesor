# Track N — Round 2 交付报告（2026-10-29）

## 改动与完成边界

基线 `d54e1c2`，分支 `nativelib`，仅在 `/Users/zhez/attesor-p2n` 工作。
已读取 N-NATIVE.md（含 Round 2）、COMMON.md、NATIVE-LIBRARIES.md 及现有实现。
未修改翻译器核心、生成代码或 TRANSLATOR_VERSION；没有安装、push、GUI、共享 Attesor 缓存或修改用户应用。

新增独立 opt-in 库；默认关闭，旧值 `1` 仍只选择 zlib：

- **vdsp**：single float `fft_zip` / `fft_zrip`，add/mul/scalar add/scalar multiply/dot/sum。
  guest 创建的真实 FFTSetup 保留给未桥接 API；宿主 companion 仅按身份索引，绝不解释 guest setup。
  companion 用 Arc 管理生命周期，同一 setup 加锁，独立 setup 可并行；未登记/宿主创建失败时回退。
- **vimage**：增加 PlanarF / ARGBFFFF scale 和 Planar8→PlanarF；原整数卷积保留。
  scratch/query 和非默认 FPCR 走 guest；整数缩放仍不替换，不放宽整数像素验收。
- **compression**：LZ4/LZFSE/zlib 一次性 encode/decode，NULL scratch；自备 scratch、其他算法和流式 API 走 guest。
  编码以跨解码器还原输入验收，不再要求两架构压缩流字节相同。
- **crypto**：一次性 CCCrypt AES，CBC/ECB、PKCS7/无 padding；其他算法/非法 operation enum 走 guest。
  密钥、IV、数据及长度指针清除 top-byte 标签；不传 CCCryptorRef。使用宿主 CommonCrypto，由系统选择 AES 优化。
- **imageio**：提供显式 `attesor_imageio_decode` / `attesor_imageio_encode` RGBX8 缓冲接口及头文件。
  编码/解码完全在宿主内部创建 CF/CG 对象，只有原始字节和固定宽度元数据跨界。
  **不是现有 CGImageSource/CGImageDestination 的透明替换**；未修改的应用仅设 imageio 开关不会提速。
  这是已验证的受限 data-in/data-out 路径，不声称完成任意 ImageIO 对象调用的加速。

浮点 shim 对非零 guest FPCR 回退。宿主数值调用临时设置默认 MXCSR，调用后恢复宿主状态；
单元测试验证 rounding/FTZ/DAZ 不泄漏。未实现跨架构 FPSR 异常状态同步。
六个 shim 均纳入 native 子进程过滤、ARM 子进程重选、构建/安装脚本；未实际运行安装。

## 验收规则与正确性实测

SDK `vecLib.framework/Headers/vDSP.h` 的 “Exactness, IEEE 754 conformance” 允许重排与末位差异，
且明确不保证 NaN/Inf 的 IEEE 行为。`vImage/Headers/Geometry.h` 说明 float scale 采用单精度过滤，
但未给出所有输入统一的跨架构 ulp 界。测试使用透明的、尺度相关的回归预算，
**不把自定阈值冒充 Apple 文档的精度保证**。特殊值测试报告实测分类一致，不推广为所有 NaN payload/FFT 特殊值的保证。

### 浮点误差（guest/off 对 bridge/on，三个种子汇总）

相对误差分母为 `max(abs(guest),abs(host),FLT_TRUE_MIN)`；
归一化误差按该记录输出最大幅度（FFT 合并 real/imag）计算，仅统计正常数幅度记录。
保留 padding 哨兵并逐元素检查，错误状态/整数结果按完整字节检查。

| API/类别 | 最大绝对误差 | 最大逐元素相对误差 | 正常数记录最大归一化误差 |
|---|---:|---:|---:|
| vadd/vmul/vsmul/vsadd、负 stride、in-place | 0 | 0 | 0 |
| dot | 1048576 | 4.83247e-6 | 4.83247e-6 |
| sum | 131072 | 2.54496e-6 | 2.54496e-6 |
| FFT/real FFT、forward/inverse | 0.6875 | 2.0 | 6.11241e-7 |
| PlanarF/ARGBFFFF scale | 7.15256e-7 | 0.00136720 | 3.97364e-7 |
| Planar8→PlanarF | 0 | 0 | 0 |

归约输入包含 1e12 量级，故绝对误差大而相对误差小。
FFT 包括连续四次**未归一化**变换；0.6875 是级联大幅度输出误差，不能当作单位输入单次 FFT 误差。
逐元素相对误差 2.0 出现在接近零/次正规分量，不能以相对误差单独判定 FFT。
次正规数的级联舍入另用与点数/级数相关的绝对下溢预算，而不是以 1 为绝对误差下限掩盖差异。
独立 double DFT 检查每种模式的 20 个 8/32 点 FFT，另检查所有尺寸的 inverse identity；
每种子 native/off/on 均通过，共 180 个独立 DFT 用例。

### 压缩、图像、AES

- 每种编码模式生成 45 个压缩样本：3 算法 ×3 数据模式 ×5 长度（1/7/31/4096/65536）。
  native、guest/off、bridge/on 及仅 compression 四种编码选择，均分别由 native/off/on 解码并完整 memcmp。
  同时验证前缀输出、空输入、损坏输入、容量不足、非法算法回退、scratch 回退及 padding/canary。
- AES 三种子均与 native x86 **完整字节**一致：128/192/256 位、CBC/ECB、padding 开关、
  0/1/15/16/17/63/64/1024 长度、NIST CBC 已知答案、in-place、NULL IV、错误密钥长度、短输出、非 AES 回退。
  对特意构造的坏 padding，实测本机返回 success/16 字节；记录并比较实际状态，未谎称系统必然拒绝坏 padding。
  开发时无效 operation=99 使 native x86 库崩溃；该非法枚举不作为有效缓冲契约测试，shim 明确回退，不包装成桥接通过。
- ImageIO：9 个尺寸/图案（1×1、17×13、256×192；渐变/棋盘/随机）分别编码 PNG/JPEG；
  native/off/on 三种编码 ×三种解码组合全通过。所有 PNG **逐像素等于原输入**；
  所有 JPEG 宿主/guest 解码和 off/on 编码后的像素差均为 **0，PSNR=∞**（只限本次样本）。
  相对原图的有损重建 PSNR 逐图保存在证据中，不把 JPEG 当无损格式。
  另外验证 padded row、忽略 X 字节、alpha PNG 拒绝、坏数据/参数、容量查询/不足及输出 canary。
- 四 pthread 覆盖共享 FFT setup、vDSP、vImage、压缩、AES；ImageIO 单独四线程。
  guest 单线程 fork 后 FFT 及 native helper→ARM exec/子进程禁用通过。
  多线程 fork 后调用非 async-signal-safe 库不在通常调用契约内，未额外承诺支持。

## L0 实际测试行

```text
TMPDIR=/private/tmp cargo build --release
  Finished release profile（既有警告仍在，无新增桥接编译错误）
cargo test --release --workspace -- --test-threads=1
  attesor: 71 passed; 0 failed
  attesor-core: 9 passed; 0 failed
  difftest/doc-tests: 0 failed
python3 tests/attesor/native-round2-check.py
  三种子 native/off/on/独立库选择：513 个完整 numeric records/次
  numeric-on: vdsp=269, vimage=18；crypto-on=177
  compression encode-on=147；decode-on=81
  四线程 on: vdsp=128, vimage=64, compression=384, crypto=64
  fork-on=1；chain 保持启用；chain-off 全部计数=0
  native-round2 suite: PASS
python3 tests/attesor/native-round2-sensitivity.py
  PASS sensitivity: private vadd -> vmul rejected
python3 tests/attesor/native-imageio-check.py
  PNG exact / JPEG error+PSNR / tagged pointers / errors / four threads
  native-imageio buffer suite: PASS
 tests/attesor/native-vimage-check.sh
  三种子、四选择、原生 helper、禁用：native-vimage suite: PASS
 tests/attesor/native-zlib-check.sh
  全部并发/标签/中间进程：native-zlib suite: PASS
bash -n native/build.sh scripts/install.sh
Python AST、clang -Wall -Wextra -Werror（新 shim/probe）、git diff --check：PASS
```

故障注入仅修改临时 shim，将 vadd 的宿主操作号从 add 换为 multiply；
实际 host 计数仍为 269，但完整比较拒绝首个错误数值（0.0316208 对 0.00000396521）。
生产源和正常 shim 没有被改坏。无 translator-core 修改，按 COMMON 使用 native 定向测试与真实工作负载，
未伪称运行 20k/100k 翻译指令自测矩阵或 translator sensitivity。

## 最终性能测量

clang -O2，原生 x86 与相同 ARM 程序，release Attesor --no-aot、私有缓存。
每个 API 先暖机；三轮严格串行 **native→off→on**。期间不并行运行自己的 QA/基准。
计时不包括启动、setup 创建、最终校验；FFT 循环包含输入清零，ImageIO 为**显式缓冲适配器**，非任意应用透明加速。
负载：dot/add 262144 float ×64，FFT 4096 点 ×64；scale 1024²→512² ×16，convert 1024² ×16；
压缩/AES 1 MiB ×16，ImageIO 256² ×8。所有轮次有计数证明宿主路径，每轮均有语义检查。

| API | x86 原生 ms（3次） | guest/off ms（3次） | bridge/on ms（3次） | 配对加速中位数 |
|---|---|---|---|---:|
| vDSP-dot-64 | [4.61, 2.589, 2.558] | [7.693848, 7.952148, 9.275879] | [3.628174, 3.579834, 4.636963] | 2.121x |
| vDSP-add-64 | [7.728, 4.628, 4.621] | [10.444092, 10.927979, 17.583008] | [5.927002, 4.556152, 7.307129] | 2.399x |
| vDSP-FFT-64 | [0.381, 0.277, 0.762] | [11.328125, 12.980957, 16.846191] | [0.705078, 0.908936, 1.282959] | 14.281x |
| vDSP-rFFT-64 | [0.473, 0.196, 0.2] | [4.367188, 3.952148, 5.037842] | [0.359863, 0.348145, 0.703125] | 11.352x |
| vImage-scaleF-16 | [8.948, 5.821, 7.019] | [352.0271, 180.61084, 237.466064] | [5.834961, 5.470947, 25.281982] | 33.013x |
| vImage-convertF-16 | [1.396, 1.392, 1.309] | [73.374023, 33.517822, 31.360107] | [1.357178, 1.312988, 11.887939] | 25.528x |
| compression-LZ4-encode-16 | [2.127, 1.483, 1.6] | [20.718018, 7.35791, 6.320068] | [1.503906, 3.848877, 2.105957] | 3.001x |
| compression-LZ4-decode-16 | [1.422, 0.724, 0.914] | [5.037109, 2.689941, 2.64502] | [0.755127, 1.193115, 1.178955] | 2.255x |
| compression-LZFSE-encode-16 | [73.936, 55.487, 59.244] | [613.873779, 257.716064, 291.035889] | [49.336914, 47.875977, 80.224121] | 5.383x |
| compression-LZFSE-decode-16 | [1.495, 1.372, 1.824] | [19.256104, 6.49292, 9.512939] | [1.222168, 1.192139, 2.195801] | 5.446x |
| compression-zlib-encode-16 | [118.604, 77.19, 73.119] | [655.208008, 243.62085, 269.973877] | [72.166016, 76.302979, 104.927002] | 3.193x |
| compression-zlib-decode-16 | [2.488, 2.03, 1.822] | [15.00708, 6.291992, 9.784912] | [1.661133, 1.793945, 2.61499] | 3.742x |
| AES-CBC-16 | [17.638, 16.27, 16.165] | [108.043945, 61.453125, 66.36499] | [17.109131, 17.809814, 17.912842] | 3.705x |
| ImageIO-buffer-PNG-encode-8 | [40.98, 26.222, 31.371] | [327.781982, 188.200928, 208.89502] | [21.932129, 23.315918, 34.057129] | 8.072x |
| ImageIO-buffer-PNG-decode-8 | [23.095, 9.384, 10.091] | [158.758057, 129.6521, 173.910889] | [12.481934, 8.957764, 13.557129] | 12.828x |
| ImageIO-buffer-JPEG-encode-8 | [11.93, 5.997, 6.525] | [157.384033, 138.7229, 218.041992] | [5.442871, 5.620117, 9.141113] | 24.683x |
| ImageIO-buffer-JPEG-decode-8 | [21.997, 10.309, 11.83] | [164.855957, 160.004883, 247.343018] | [10.331055, 10.556152, 14.811035] | 15.957x |

最终逐次 uptime（保留繁忙机器上的异常值，不挑最好一轮）：

```text
native-1: 22:45  2 users, load averages: 18.67 11.41 9.45
off-1: 22:45  2 users, load averages: 18.67 11.41 9.45
on-1: 22:45  2 users, load averages: 21.02 12.02 9.68
native-2: 22:45  2 users, load averages: 19.90 11.93 9.66
off-2: 22:45  2 users, load averages: 19.90 11.93 9.66
on-2: 22:45  2 users, load averages: 19.90 11.93 9.66
native-3: 22:45  2 users, load averages: 19.90 11.93 9.66
off-3: 22:45  2 users, load averages: 19.90 11.93 9.66
on-3: 22:45  2 users, load averages: 21.67 12.43 9.85
```

最终 load 18.67→21.67，明显繁忙，vImage on 第三轮尤其有抢占异常。
这不是无争用性能承诺。FFT 暖机后 off/on 约14×，不复述 Round 1 冷调用/不同循环中的231×为本次桥接收益。
另保留修改 FPCR/MXCSR 防护前的探索性三轮原始记录，但不将其用于当前候选验收。

## 无 GUI 实际工作负载

COMMON 指定的 DeepSeek Harness Electron，以 `ELECTRON_RUN_AS_NODE=1` 跑现有 js.js 和 net/t.js；
0 与全部六库选择各一次，均退出0。JS 七项结果相同：
`28665, -259384288, 80000, 400000, 2025151008, 19360, 6448448`。
SHA256/SHA512、GCM/tag、ECDSA/ECDH、Unicode 输出一致。
两种模式 TLS 都是 **https error ENOTFOUND**，未达到联网验收的 `https status 401`。
同样保留 Electron 的 codesign task_name_for_pid 权限警告；未把权限/DNS 问题改写为通过。
这些仅是无 GUI 共存烟雾测试，不证明 Electron 调用了本次全部 API，也不把单轮 JS 时间当作收益。

## 拒绝/延期、默认启用建议与开放风险

| 范围 | 决定/风险 |
|---|---|
| 整数 scale / 未实现整数转换 | 保留 guest；已知像素差仍不满足整数精确政策，SDK 未给本次可采用的宽松整数容差。 |
| vDSP double、2D、DFT、out-of-place 等 | 未桥接；保留真实 guest setup 以兼容。setup 双份内存与创建成本未计入内核 benchmark。 |
| float 精度与环境 | SDK 无统一数值保证；当前是样本误差/独立 oracle 验收，不是全输入证明；非默认 FPCR 回退，FPSR 异常状态未同步。 |
| scratch / streaming / opaque state | 保留 guest，不混用架构私有 scratch 尺寸、z_stream、compression_stream、CCCryptorRef。 |
| ImageIO 通用对象路径 | **延期**。不能将 guest CF/CG 对象、回调和生命周期直接交给 host；本次仅显式 RGBX8 sRGB 接口。 |
| alpha / metadata / EXIF orientation / animation / HEIC / HDR | 不在缓冲接口支持范围；alpha 明确拒绝；其余不冒充完整 codec 覆盖。 |
| CoreFoundation dictionaries/strings | 没有选出不含不透明对象/回调且有收益的纯数据热路径，拒绝直接替换。 |
| SHA256 / BNNS | 前者 Round 1 ARM 已快于 host；后者并非 trivial，无本次实现/新测量。 |

**现在可无条件默认开启的库：无。** 全部仍 opt-in，未修改默认开关。
按纯数据 ABI、字节/互操作验证和收益，**compression 与 crypto 是下一批默认开启优先候选**，
但仍需 Claude 的 L1/L2、其他 macOS/sysroot 版本及真实调用工作负载确认。
vdsp/vimage float 需额外精度、环境与应用接受度审阅；imageio 必须先由应用显式采用，不能作为通用默认加速宣传。
既有 zlib/integer convolution 默认策略不变。所有路径仍要求原库的有效地址、长度、非别名等调用契约；不是非法指针隔离层。

L1 全类100k、loopra-AOT、gdbstub、可联网 TLS401，以及 L2 多 seed/GUI/真实应用/长时间并发与多系统版本，
未在本次执行；按 COMMON 由合并/default-on 验收承担。没有因本次 L0 通过宣布达到 release/default-on 验收。

## 证据与提交交付

完整测试输出、误差汇总、两组原始测量（标明版本阶段）、逐次 uptime、计数与 headless stdout/stderr：
[`measurements/2026-10-29-n-native-round2.txt`](../../../measurements/2026-10-29-n-native-round2.txt)。
详细日志、二进制样本与私有缓存位于 `target/native-round2-*` / `target/native-imageio-check` / `target/n-native-r2-*`。

提交状态及可应用补丁见交付末尾记录。

`git add` 被只读 worktree 元数据阻止，无法创建 `index.lock`（Operation not permitted）。
未提交、未 push；修改完整保留在本工作树。按 COMMON 保存单个逻辑步骤的提交说明和补丁：

- `target/n-native-round2-commit-message.txt`
- `target/n-native-round2.patch`（包括所有新增源码、测试、文档和测量证据）
