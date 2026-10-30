# Track N — 原生系统库替换报告（2026-10-29）

## 结果与范围

- 分支 `nativelib`，工作树 `/Users/USER/attesor-p2n`，基线 `e1efa68`。
- **先测量，后选 API**：最初三轮原生/翻译交替测量在桥接实现前完成。
  初测缩放约 71.42×、卷积 29.34×、FFT 324.46×；不是直接按最大数字实施。
  更广的字节对比否决了缩放、FFT 和 LZ4 编码，最终仅桥接
  `vImageConvolve_Planar8` 的 NULL 临时缓冲区调用。
- `ATTESOR_NATIVE_LIBS=vimage` 独立选择；`zlib,vimage` 可组合；默认关闭。
  `1` 保持旧含义：仅 zlib。未知名字明确失败，不默默忽略。
- 新增 ARM 插入库 `libattesorvimage.dylib`，在 `native.rs` 中调用宿主 Accelerate。
  将 11 个 C 参数装入 10 个固定宽度槽（NULL scratch 隐含），复制 LP64 描述符，
  清除描述符、内嵌像素指针和 kernel 的 top-byte 标签。
  没有 ObjC/CF 对象、客体回调或共享临时缓冲区。
- `kvImageGetTempBufferSize` 与非 NULL scratch 走原始 ARM 实现；不得混用宿主私有缓冲区布局。
  每个库独立诊断计数；原生中间进程移除两种 ARM shim，ARM 子进程重新按选项注入。
- 更新构建/安装脚本（**没有运行安装**），增加基准、字节差分、故障注入和测量驱动器。
- **未修改 `crates/core`、`emit.rs`、`jit/`、翻译版本或生成代码。**
  没有 GUI、push、修改应用或共享用户缓存；只启动自己的测试进程。

## 环境与测量方法

宿主 x86-64 macOS 26.7.2（25G332），ARM sysroot 目录 `26.6.2__Mac14,2`。
两个系统库版本不同，因此观察到的字节差异不能单独归因于指令集。
全部程序 `clang -O2 -arch arm64/x86_64`，翻译使用本工作树 release 二进制、`--no-aot`，
所有 `ATTESOR_CACHE` 为工作树/本次临时目录中的私有路径。
每轮严格按原生→翻译（或原生→关闭→开启）执行，至少三轮，不并行跑自己的基准。
机器仍受其他工作负载影响；保留异常值，不据此开启默认加速。

初测原始记录保留在下述证据文件。之后修正 survey 的计时/哈希求值顺序：
先固定结束时间再计算校验哈希，避免 C 参数求值顺序不明确。
以下是修正后的独立三轮复测；初测和复测都支持同一个候选选择。
测试参数：libcompression 1 MiB ×4，SHA256 1 MiB ×16，AES-CBC 1 MiB ×8；
卷积 1024² ×4，缩放 1024²→512² ×4，dot 262144 个 float ×64，
4096 点 FFT ×16，CF 字符串/字典 4096 条 ×4，ImageIO 256² 编解码 ×2。
没有用单个哈希相同代替桥接正确性验收；哈希仅用于基准快速检查。

### 各 API 的原生/翻译开销（桥接关闭）

| API | x86 原生 ms（三次） | ARM 翻译 ms（三次） | 中位数之比 |
|---|---|---|---:|
| compression-LZFSE-encode | 11.115, 12.928, 10.952 | 66.322, 85.940, 65.188 | 5.97× |
| compression-LZFSE-decode | 0.965, 0.828, 0.924 | 5.024, 5.360, 5.705 | 5.80× |
| compression-LZ4-encode | 0.403, 0.425, 0.416 | 3.803, 5.050, 4.129 | 9.93× |
| compression-LZ4-decode | 0.273, 0.267, 0.234 | 2.806, 3.530, 3.521 | 13.19× |
| compression-zlib-encode | 17.387, 16.007, 16.989 | 62.828, 86.731, 76.054 | 4.48× |
| compression-zlib-decode | 0.652, 0.727, 0.589 | 6.864, 7.100, 7.566 | 10.89× |
| CommonCrypto-SHA256 | 50.274, 55.151, 48.396 | 37.287, 41.854, 38.581 | 0.77× |
| CommonCrypto-AES-CBC | 9.502, 9.457, 8.204 | 28.075, 31.012, 33.264 | 3.28× |
| vImage-convolve | 1.215, 1.264, 1.187 | 33.012, 39.004, 36.925 | 30.39× |
| vImage-scale | 1.325, 1.366, 1.413 | 78.707, 92.803, 102.771 | 67.94× |
| vDSP-dot | 3.071, 2.776, 3.645 | 8.573, 8.846, 9.699 | 2.88× |
| vDSP-FFT | 0.155, 0.088, 0.122 | 28.199, 27.047, 34.264 | 231.14× |
| CoreFoundation-dictionary-string | 15.080, 16.390, 14.322 | 105.300, 102.303, 109.489 | 6.98× |
| ImageIO-PNG-encode | 7.077, 7.853, 6.329 | 103.192, 110.524, 111.188 | 15.62× |
| ImageIO-PNG-decode | 4.445, 4.403, 3.901 | 78.240, 88.807, 93.500 | 20.17× |
| ImageIO-JPEG-encode | 1.595, 1.666, 1.520 | 50.810, 60.503, 53.456 | 33.51× |
| ImageIO-JPEG-decode | 1.790, 1.728, 1.789 | 45.567, 55.881, 48.078 | 26.87× |

HEIC：两种架构的 ImageIO 编码均失败，记录为 `SKIP encoder-unavailable`，
没有报告虚假的时间/比率。由于未取得独立 HEIC 样本，解码也未测得；
这**不代表解码器不存在**。BNNS 为可选项，本次未展开不平凡的模型/不透明 filter 配置。

最终 survey 六次运行的 `uptime`（顺序 native-1/off-1/native-2/off-2/native-3/off-3）：

```text
22:18  2 users, load averages: 5.98 9.62 10.86
22:18  2 users, load averages: 6.06 9.57 10.84
22:18  2 users, load averages: 6.06 9.57 10.84
22:18  2 users, load averages: 6.06 9.57 10.84
22:18  2 users, load averages: 6.06 9.57 10.84
22:18  2 users, load averages: 6.06 9.57 10.84

```

### 已实现卷积桥接的 A/B

每次测量前暖机一次；计时 128 次 1024²、3×3 整数均值卷积，不含启动、输入初始化和校验。
同一候选二进制关闭/开启交替运行；两轮系列的全部原始数值都保留：

| 轮次 | x86 原生 ms | 桥接关闭 ms | vimage 开启 ms | 关闭/开启 |
|---|---:|---:|---:|---:|
| ab-1 | 27.007 | 967.868 | 36.810 | 26.29× |
| ab-2 | 32.796 | 952.302 | 39.014 | 24.41× |
| ab-3 | 36.081 | 1050.795 | 43.365 | 24.23× |
| ab 配对中位数 | — | — | — | 24.41× |
| ab-final-1 | 41.460 | 1588.562 | 30.809 | 51.56× |
| ab-final-2 | 30.329 | 942.903 | 32.902 | 28.66× |
| ab-final-3 | 26.868 | 932.980 | 30.577 | 30.51× |
| ab-final 配对中位数 | — | — | — | 30.51× |

全部 18 次原生/关闭/开启运行输出同一 `1048576` 字节结果的哈希
`d944f4516193b031`；每次开启的宿主计数为 **129**（含暖机），关闭/原生为 0。
第一轮 load averages 约 14.64/15.76/12.88 至 15.31/15.88/12.94；
第二轮的逐次 `uptime` 如下，第一轮各行及所有 stderr 见完整证据：

```text
22:19  2 users, load averages: 10.11 10.20 10.97
22:19  2 users, load averages: 10.11 10.20 10.97
22:19  2 users, load averages: 11.95 10.58 11.10
22:19  2 users, load averages: 11.95 10.58 11.10
22:19  2 users, load averages: 11.95 10.58 11.10
22:19  2 users, load averages: 11.95 10.58 11.10
22:19  2 users, load averages: 11.39 10.48 11.06
22:19  2 users, load averages: 11.39 10.48 11.06
22:19  2 users, load averages: 11.39 10.48 11.06

```

复现命令（不要安装到用户目录）：

```sh
python3 tests/attesor/native-libraries-measure.py --out target/native-survey
python3 tests/attesor/native-libraries-measure.py --bridge --out target/native-ab
```

## 验证命令与实际结果

```text
cargo build --release
  Finished release profile（既有警告仍在，未为消除警告改动其他子系统）
cargo test --release --workspace -- --test-threads=1
  attesor: 68 passed; 0 failed
  attesor-core: 9 passed; 0 failed
  difftest/doc-tests: 0 failed

tests/attesor/native-vimage-check.sh
  PASS bytes seed=1          libraries=0/vimage/zlib/zlib,vimage
  PASS bytes seed=42         libraries=0/vimage/zlib/zlib,vimage
  PASS bytes seed=0xdeadbeef libraries=0/vimage/zlib/zlib,vimage
  每次开启 bridge-calls=136；关闭或仅 zlib 时 =0
  PASS --chain
  PASS --chain-off
  native-vimage suite: PASS

tests/attesor/native-zlib-check.sh
  bridge-calls=0 / 4332；完整四线程、标签指针和中间进程测试通过
  native-zlib suite: PASS

tests/attesor/native-vimage-sensitivity.sh
  native-vimage: PASS seed=1 bridge-calls=136
  PASS sensitivity: divisor+1 rejected; 1087955/1394517 bytes differ
```

vImage 正式差分使用 `cmp` 比较全部输出字节（包括 padding、错误返回值、错误时输出哨兵），
不是只比较结果摘要。三个种子各覆盖：16 种 1..7 奇数 kernel 尺寸组合、
正负及 int16 极值系数、0/正/负除数、四种边缘模式、平铺开关、ROI、
标签描述符/像素/kernel、客体 scratch、四个独立 pthread 及 1 MiB 内部平铺图像。
7 个错误用例包括偶数 kernel、X/Y 越界、NULL kernel、无边缘模式、冲突模式、目的图过大。
缺失 shim 和未知选择必须在启动前失败；原生 helper 跳板及子进程禁用都通过。

灵敏度脚本只改变临时目录的 shim 副本，故意将 divisor 传成 divisor+1；
宿主路径仍被调用，程序自身检查仍退出 0，但独立字节比较检出 1,087,955 个错误字节。
生产源码/正常 shim 不被改坏，不涉及翻译器突变。
开发中还修正过测试自身两处错误假设：未知 scale flag 实际被原生忽略；
并发时不能对全局调用计数逐次断言不变。最终只在串行阶段检查 scratch 不增计数，
在线程 join 后检查准确总数；未把早期失败当成通过。

其他检查：`bash -n`、Python AST 解析、ARM shim/probe 的 `clang -Wall -Wextra -Werror -fsyntax-only`、
`git diff --check` 均通过。安装脚本仅做语法检查，未运行用户安装。

### 无 GUI 实际工作负载

按照 COMMON 用 `ELECTRON_RUN_AS_NODE=1` 启动 DeepSeek Harness 的 Electron：
`~/attesor-dshprof/js/js.js` 与 `net/t.js`，各在 0/vimage 模式运行一次，均退出 0。

- JS 各项结果相同：`28665, -259384288, 80000, 400000, 2025151008, 19360, 6448448`。
  这里只是运行烟雾/结果检查，没有把单轮 JS 时间当作桥接收益。
- SHA256 `ca6073392ee71dbd`，SHA512 `fcf47d64f5ceadd9`，
  GCM `89788ae8c19a5009` / tag `de047988105bcc38`，ECDSA/ECDH 均 true，Unicode 输出一致。
- TLS 两种模式都输出 **`https error ENOTFOUND`**，因此未达到联网环境要求的
  `https status 401`；网络验收待有 DNS/网络条件时重跑，不能记为 TLS 联网通过。
- 这些 headless 工作负载不证明真实应用调用了本次卷积桥接，也没有 GUI 验收。

## 拒绝或延期的 API

| API | 决定与原因 |
|---|---|
| vImageScale_Planar8 | **拒绝桥接**。初测 1024→512 相同不足以验收。固定 seed=1、60×12→13×27、stride 67→24、DoNotTile 的反例中，648 字节有 156 字节不同。保留 `native-libraries-bench.c rejected-scale` 原始字节复现模式。 |
| vDSP FFT | **拒绝桥接**。初测和复测原生/ARM 结果哈希分别为 `b7bdeea03601e99e` / `21099118fd5c1959`；FFTSetup 又是不透明的架构私有状态，不能直传。 |
| libcompression LZ4 encode | **拒绝按字节等价替换**。同一输入输出哈希 `2b9fec9bda6b50c2` / `84ce2c3d220c483e`，虽然解码结果相同。 |
| LZFSE/zlib encode/decode、LZ4 decode | 延期；这些样本可互操作，但低于选中卷积的收益。caller scratch 需要独立尺寸/版本策略；不能以单一输入判定全部编码流相同。 |
| ImageIO PNG/JPEG | 不直接桥接：CGImage/CFData/色彩空间等对象含客体实现状态，不能原样跨架构解释，也不在本次纯缓冲 C ABI 内。 |
| ImageIO HEIC | 编码不可用，缺独立解码样本，未给性能比率。后续需要样本/可用 encoder，不编造结果。 |
| CoreFoundation 字典/字符串 | 不直接桥接：不透明 CF 对象、回调/allocator、生命周期不兼容，违反无客体回调边界。 |
| vDSP dot | 延期：收益较小；本次有限输入相同，不代表所有浮点归约顺序可字节等价。 |
| CommonCrypto SHA256 | 不选：翻译版已比 x86 原生快；没有合理的性能收益。 |
| CommonCrypto 一次性 AES-CBC | 延期：测得约 3.28×，低于卷积；上下文/流式接口不跨界。 |
| BNNS | 可选项未测；涉及 filter/描述符与模型生命周期，本次不认定为 trivial。 |
| vImage caller scratch/query、其他像素格式/转换器 | 明确不加速，保留原 ARM 实现；其余接口没有被导出替换。 |

## 开放风险、验收边界

1. 本次结论仅限实测宿主/ARM sysroot；不同系统版本、其他 kernel/尺寸仍需额外验证。
   缩放反例说明“图像看起来差不多”和简单随机循环都不足以满足字节验收。
2. 非 NULL scratch 路径没有提速；内部 host 分配的开销可能使小图得不偿失。
   仍要求 vImage 的有效指针、非别名、长度及累加器范围契约；不是非法内存隔离层。
3. 机器繁忙，128 次调用的原始开销有波动，两轮配对中位数 24.41× / 30.51×，
   不推广为所有应用收益，也不改默认开关。
4. 根据当前 COMMON 的分层 QA，本次为 native 子系统 L0 + 定向测试 + headless 实际工作负载。
   未修改翻译路径，所以没有伪称跑过完整 translator 自测矩阵。
   L1 全类 100k（开/关）、loopra-AOT、gdbstub 及可联网 TLS 401 仍由合并验收执行；
   L2 GUI/真实应用与多版本、长时间并发测试也未执行。最初读取的旧版 COMMON 随后已更新为此分层规则。
5. 默认启用仍须 Claude 测量并接受；本次只交付 opt-in 候选，不声称已完成 release/default-on 验收。

## 证据与提交交付

完整原始数值、逐次 `uptime`、调用计数、测试和 headless stdout/stderr：
[`measurements/2026-10-29-n-native.txt`](../../../measurements/2026-10-29-n-native.txt)。
本地详细构建日志与临时实验保存在 `target/n-native/`。

`git add` 被沙箱阻止：无法创建本工作树的 `index.lock`（Operation not permitted）。
没有提交或 push；按 COMMON 保留工作树修改，并保存两个逻辑步骤的消息和补丁：

- `target/n-native-01-commit-message.txt` / `target/n-native-01-measurement.patch`
- `target/n-native-02-commit-message.txt` / `target/n-native-02-vimage.patch`
- `target/n-native-commit-message.txt` 为交付索引。
