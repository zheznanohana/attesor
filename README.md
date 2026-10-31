# Attesor

**在 Intel Mac 上运行 arm64 的 macOS 应用。**

Attesor 是一个"反向 Rosetta"。它加载 arm64 的 Mach-O 程序，连同真正的 arm64 macOS 用户态（dyld 和 arm64e 的 dyld shared cache，从 Apple Silicon 的系统镜像里取出），然后用解释器加 x86-64 JIT 执行 arm64 代码。系统调用和 Mach trap 在语义一致时直接交给 Intel 主机内核，不一致时由 Attesor 模拟。Metal 调用可以桥接到 Intel Mac 的真实显卡上。

苹果已经停止支持 Intel Mac，越来越多的新应用只发布 arm64 版本。Attesor 让这些应用继续在 Intel Mac 上运行。

[English README](README.en.md)


## 能运行什么

| 程序 | 状态 |
|---|---|
| DeepSeek Harness（Electron） | 可用，日常使用的主力测试应用，冷启动约 35 秒就绪 |
| VS Code arm64（Electron） | 可用，GPU 经 ANGLE/Metal 走 RTX 3060 |
| 命令行程序 | 可用：hello world、Node |
| AppKit / SwiftUI 程序 | 可用：Codable、actor、task group、Regex、SwiftUI 状态与 `.task` |
| `CAMetalLayer` / `MTKView` | 可用，60/60 帧上屏 |
| 音频 | 可用：AudioQueue、HAL IOProc、AVAudioEngine、AVAudioPlayer |
| 沙盒应用 | 容器路径与原生一致（`~/Library/Containers/<id>/Data`） |
| Metal | `supportsFamily:` 与设备限制与原生一致；光线追踪和 MetalFX 结果逐位一致 |
| VideoToolbox | H.264 / HEVC 编解码往返正确（软件编码） |
| IOKit | 平台信息、电源、HID 管理器等基础查询 |

不支持：FairPlay/DRM 加密的程序（App Store 和 iOS-on-Mac 应用），以及只有 arm64e 切片的程序。

## 性能

以下都是在同一台 Intel Mac 上实测的结果。"原生速度"指同一份源码编译成 x86-64 后直接运行的速度。

### CPU 内核

同一份 C 源码分别编译成 arm64 版（在 Attesor 里运行）和 x86-64 原生版，交替测量 9 轮取中位数：

| 内核 | 类型 | 达到原生速度 |
|---|---|---|
| list | 链表遍历 | 115% |
| memcpy | 内存复制 | 104% |
| int-hash | 整数哈希 | 101% |
| crc32 | 位运算 | 81% |
| memops | 内存操作 | 76% |
| sieve | 筛法 | 63% |
| branchy | 分支密集 | 63% |
| qsort | 排序（大量函数调用） | 49% |
| matmul | 浮点矩阵乘 | 31% |
| fib(36) | 递归调用 | 24% |
| fp-nbody | 浮点 n 体模拟 | 21% |
| **几何平均** | | **57%** |

整数、内存和位运算代码跑到原生速度的 75% 到 115%；浮点运算和深度递归调用目前在原生速度的 20% 到 50% 之间。

### 微基准

| 项目 | 相对原生的耗时 |
|---|---|
| 整数循环（3 亿次） | 1.04 倍 |
| 系统调用（10 万次） | 1.0 倍 |
| 浮点循环（5 千万次） | 1.37 倍 |
| memcpy 带宽 | 1.6 倍 |
| 混合套件 | 约 1.5 倍 |
| JavaScript（V8） | 约 5 倍 |

系统调用直接交给主机内核执行，几乎没有额外开销。

### 应用

| 场景 | 耗时 |
|---|---|
| DeepSeek Harness 冷启动到界面就绪 | 约 35 秒 |
| 用 DSH 内置 Electron 运行 `dsh --version`（缓存已生成） | 约 3 秒 |
| `node --version`（arm64 Node） | 约 5 秒 |
| 命令行 hello world | 约 0.9 秒 |

首次运行一个应用时需要翻译代码，会明显慢一些；正常退出后翻译结果会保存到缓存，之后的启动直接加载。

测量脚本和 guest 源码在 [`bench/`](bench/)（`bench.c`、`native-ratio/kernels.c`），应用启动测量用 [`bench/app-startup.sh`](bench/app-startup.sh)。

## 快速开始

需要：

- Intel Mac，macOS 26（Tahoe）
- Xcode Command Line Tools 和 Rust 工具链
- 约 10 GB 磁盘空间，以及一次性下载 arm64 dyld shared cache 的网络

```bash
git clone https://github.com/zheznanohana/attesor.git
cd attesor
scripts/install.sh          # 编译 attesor 和 GPU 桥接，安装到 ~/bin
scripts/fetch-sysroot.sh    # 下载 arm64e dyld shared cache（约 5.5 GB）到 ~/attesor-sysroot
attesor ./some-arm64-binary # 直接运行，sysroot 会自动找到
```

`fetch-sysroot.sh` 用 [`ipsw`](https://github.com/blacktop/ipsw) 从苹果的更新服务器下载 Apple Silicon 系统镜像并取出 dyld shared cache。更多安装细节见 [docs/SETUP.md](docs/SETUP.md)。

## 像普通应用一样双击运行

`launcher/attesor-apps` 给只有 arm64 的 `.app` 加一个 x86-64 启动切片。加完之后，Finder、Dock、Spotlight 里双击就会自动用 Attesor 运行，原来的 arm64 代码保持不变。

```bash
attesor-apps scan            # 扫描 /Applications，给所有纯 arm64 应用加启动切片
attesor-apps inject APP      # 只处理一个应用（别名 wrap）
attesor-apps restore APP     # 还原成原始的 arm64 应用
attesor-apps list            # 列出已处理的应用
attesor-apps reinject        # 升级所有已处理应用的启动切片（别名 rewrap）
attesor-apps fixdock         # 修复 Dock 图标
```

- 安装 `launcher/local.attesor-apps.plist` 为 LaunchAgent 后，每 30 秒自动扫描一次，新装或更新的 arm64 应用会被自动处理。
- 应用更新后 arm64 部分会被保留，替换操作是原子的，失败时原应用不受影响。
- 正在运行的应用不会被修改。
- 扫描目录可以用 `ATTESOR_SCAN_DIRS`（冒号分隔）指定。
- 运行日志写在 `~/Library/Logs/Attesor/<应用名>.log`。

Electron/Chromium 应用会自动加上 `--no-sandbox` 和 GPU 桥接参数，其他应用直接带 GPU 桥接运行。设 `ATTESOR_APPS_GPU=0` 改用软件渲染。

## 运行常见应用

### VS Code

下载 [darwin-arm64 版](https://update.code.visualstudio.com/latest/darwin-arm64/stable)，解压到 `~/vscode-arm64`：

```bash
vscode-gpu   # 也装成了 `code`；用 VSCODE_DIR / VSCODE_DATA 改位置
```


### 任意 Electron / Chromium 应用

```bash
attesor --gpu --gpu-only --type=gpu-process ./App --no-sandbox --disable-gpu-sandbox --disable-gpu-watchdog \
  --use-angle=metal --use-gl=angle --enable-gpu --ignore-gpu-blocklist --disable-angle-features=disableMetalOnNvidia
```

不用 GPU 时：`attesor ./App --no-sandbox --disable-gpu`。

### 命令行工具和 Node

```bash
attesor ./hello
ELECTRON_RUN_AS_NODE=1 attesor ".../Code" -e "console.log(process.version)"
```

### 使用建议

- 程序必须是 arm64 Mach-O（`file app` 显示 `arm64`）。
- 二进制提示签名问题时，先重签一份：`codesign -f -s - file`。
- 首次启动要翻译代码，会慢一些。`attesor --prewarm App.app` 可以不执行应用、提前把主程序、helper 和 dyld 翻译好；正常退出时也会保存翻译缓存，第二次启动更快。
- 翻译器版本升级后旧缓存会自动作废。

## 命令行参数

```
attesor [选项] ./arm64-程序 [参数...]
```

| 参数 | 作用 |
|---|---|
| `--prewarm` | 静态翻译可执行文件或 `.app` 以及 dyld，不运行程序（别名 `--aot`） |
| `--gpu` | 把程序的 Metal API 转到主机显卡 |
| `--gpu-only SUBSTR` | 只给参数里含 SUBSTR 的进程启用 GPU（如 `--type=gpu-process`） |
| `--sysroot DIR` | 指定 arm64e dyld shared cache 所在目录（默认自动查找） |
| `--dyld PATH` | 指定 arm64 dyld（默认 `/usr/lib/dyld`，或 `$ATTESOR_DYLD`） |
| `--no-aot` | 不读写翻译缓存 |
| `--interp` | 只用解释器，不用 JIT/AOT，用来排查 JIT 问题 |
| `--trace` | 记录系统调用、Mach 消息和路径重定向 |
| `--dump-on-fault` | 崩溃时把寄存器和反汇编写到 `/tmp/attesor-fault-<pid>.log` |
| `--gdb PORT` / `--gdb-fd FD` | 用 lldb/gdb 调试第一个 guest 线程 |
| `--selftest-jit CLASS N [SEED]` | JIT 差分自测（见下文） |
| `--jit-dump HEX...` | 打印一段 arm64 指令翻译出的 x86 机器码 |

### 本机整合版默认配置（codegen106）

本机整合版的 `attesor` 默认开启已保留的优化配置；不再需要换用 `attesor-fast`。
`attesor-fast` 保留为同配置的 JIT-only 入口，`attesor-safe` 可强制退回此前的保守配置。
失败实验仍关闭；这不代表所有应用或 V8 都已验证加速。显式 `ATTESOR_JIT_ON/OFF` 仍可覆盖默认选择。
首次启动可能重建翻译缓存。详见 [本机默认开启说明](research/rosetta/LOCAL-DEFAULTS106.md)。

## 环境变量

| 变量 | 作用 |
|---|---|
| `ATTESOR_JIT_ON` | 逗号分隔的优化开关列表，**设置后整体替换默认列表**（设为空字符串即全部关闭） |
| `ATTESOR_JIT_OFF` | 逗号分隔，关闭指定开关，优先级最高 |
| `ATTESOR_CACHE` | 翻译缓存目录 |
| `ATTESOR_SYSROOT` | dyld shared cache 目录 |
| `ATTESOR_STATS=1` | 退出时打印 JIT 统计：翻译块数、翻译耗时、解释执行次数 |
| `ATTESOR_PROFILE=DIR` | 约 1 kHz 采样所有 guest 线程，用 `tools/profile-report.py DIR` 汇总 |
| `ATTESOR_JIT_THRESHOLD` | 块执行多少次后编译（默认 24） |
| `ATTESOR_NATIVE_LIBS=1` | 启用主机 zlib 桥接 |
| `ATTESOR_NO_CONTAINER=1` | 沙盒应用不使用容器路径 |
| `ATTESOR_NO_AOT_BUILD=1` | 不在退出时生成 AOT 缓存 |
| `ATTESOR_STDERR_LOG=1` | 每个 guest 进程的 stderr 另存到 `~/Library/Logs/Attesor/stderr/` |

完整列表见 [docs/DEBUGGING.md](docs/DEBUGGING.md)。

## 优化开关

默认开启的 18 项，都在 DeepSeek Harness 启动测试中验证过不会拖慢：

| 开关 | 作用 |
|---|---|
| `loopra` | 循环内的整数寄存器常驻宿主寄存器，循环和分支密集代码的主要加速来源 |
| `flagreg` | 循环回边上的 NZCV 标志保存在寄存器里，延迟物化 |
| `isel2` | 用 BMI2 变量移位和 LZCNT 生成更短的指令 |
| `notbi` | 能证明地址高字节为零时省掉 TBI 掩码 |
| `fastenc` | MOV 和整数 ALU 指令直接编码，跳过通用汇编器 |
| `slotreuse` | 复用链接槽的对齐重定位 |
| `regioncache` | 自然循环的写穿寄存器缓存 |
| `bgtrans` | 后台线程翻译 |
| `nativefixups` | 用原生代码完成 dyld 重定位（Electron Framework 65 万条 rebase 约 9 ms） |
| `pathcache` | dyld 路径映射缓存 |
| `offline-constprop` | 离线 AOT 常量传播 |
| `offline-dse` | 离线 AOT 死存储消除 |
| `offline-layout-profile` | 按真实采样权重排列 AOT 代码 |
| `preciseinvalidate` | 精确的代码失效，无关的内存映射不会清掉热 AOT 代码 |
| `process-qos` | 后台翻译线程降低 QoS，前台线程不变 |
| `avx512`、`avx512nan` | 主机支持 AVX-512 时用到高 16 个 xmm 寄存器 |
| `hugepages` | 代码缓存申请 2 MiB 大页，失败时自动回退 |

默认关闭、可以手动开启的：

| 开关 | 默认关闭的原因 |
|---|---|
| `tiered` | 分层 JIT，DSH 启动时间明显变长 |
| `rel32chain` | 不可变 rel32 直接链接，DSH 启动慢 21% |
| `ibtc2` | 两路间接跳转缓存，DSH 启动慢 19% |
| `sharedtrans` | 进程间共享翻译，DSH 启动慢 10% |
| `bliccache` | BLR 站点缓存，DSH 启动慢 8% |
| `loopra2` | 多块循环寄存器分配，与其他开关组合时启动变慢 |
| `page16k` | 向 guest 报告 16 KiB 页，DSH 启动会中止 |
| `callret` | 宿主 call/ret，JS 慢 12%、启动慢 16% |
| `jitcost`、`loopcensus` | 诊断用的计数和统计 |

例子：

```bash
ATTESOR_JIT_OFF=loopra attesor ./app           # 默认配置去掉 loopra
ATTESOR_JIT_ON=loopra,isel2 attesor ./app      # 只开这两项
ATTESOR_JIT_ON= attesor ./app                  # 全部关闭
```

## 工作原理

1. **加载**：解析 arm64 Mach-O，映射 arm64 dyld，把 dyld shared cache 映射到 guest 地址空间；预滑动后的 shared cache 数据页会缓存在磁盘上，加快之后的启动。
2. **执行**：代码先由解释器运行，执行次数达到阈值的基本块交给 JIT 翻译成 x86-64；热循环会整体翻译成带寄存器分配的循环区域。
3. **缓存**：翻译结果按 AOT 格式写到磁盘，下次启动直接加载；`--prewarm` 可以提前生成。
4. **系统接口**：BSD 系统调用、Mach trap 和 machdep 调用直接转给主机或模拟；commpage、16 KiB/4 KiB 页面差异、沙盒容器路径都在这一层处理。
5. **GPU**：guest 的 Metal 调用经桥接库发给主机 Metal 设备；Electron 应用的 ANGLE/Metal 就是这样拿到硬件加速的。
6. **原生库**：zlib、vImage、压缩、加密等热点函数可以直接调用主机版本。

详细设计见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) 和 [docs/GPU.md](docs/GPU.md)。

## 测试与调试

JIT 差分自测把同一段随机生成的 arm64 代码分别交给解释器和 JIT 执行，逐位比较寄存器、标志和内存：

```bash
attesor --selftest-jit dpimm 100000        # 数据处理（立即数）
attesor --selftest-jit fpblocks 100000     # 浮点块
attesor --selftest-jit loops 300000 0x1234 # 循环区域，带种子
```

常用类别：`dpimm dpreg ldst branch blocks fpblocks fuse fuse2 selchain tbiblocks fpnan calls jitcode offline addrforms loops`。所有类别都应输出 `mismatch=0`。

```bash
cargo test --release --workspace -- --test-threads=1   # 单元测试
python3 tests/attesor/accept-all.py --output out.json  # 21 组自动验收
bench/app-startup.sh --runs 3 'dsh web: http' "<应用可执行文件>" --no-sandbox --disable-gpu   # 应用启动计时
```

`tests/attesor/` 里还有 AOT 缓存、加密、调试器、原生库、启动器、FP16、FPCR 等专项检查脚本。

调试器：`attesor --gdb 1234 ./app` 后用 `lldb` 执行 `gdb-remote 1234` 连接。

## 目录结构

| 路径 | 内容 |
|---|---|
| `crates/attesor/src/` | 主程序：加载器、系统调用、线程、信号、GPU、AOT |
| `crates/attesor/src/jit/` | JIT：`emit.rs` 代码生成，`loops.rs` 循环区域，`offline.rs` 离线 AOT，以及各优化开关 |
| `crates/core/` | guest CPU 状态、参考解释器、软件浮点、反汇编，JIT 与 AOT 共用 |
| `gpu/` | Metal 桥接库和 Chromium 启动脚本 |
| `native/` | 原生库桥接 |
| `launcher/` | `attesor-apps` 应用包装器和 x86 启动切片 |
| `scripts/` | 安装、下载 sysroot、VS Code / Etcher 启动脚本 |
| `bench/` | 基准程序和测量脚本 |
| `tests/attesor/` | 验收与专项检查 |
| `tools/` | profiler 汇总、窗口列表、DevTools 客户端等工具 |
| `docs/` | 设计、性能、调试和各项特性的文档 |
| `ref/` | 参考用的苹果开源代码节选（xnu、dyld、libpthread、libdispatch），不参与编译 |

## 文档

| 文档 | 内容 |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | 环境要求、编译、获取 arm64 用户态、安装 GPU 桥接 |
| [docs/USAGE.md](docs/USAGE.md) | 参数、各类应用的启动方式 |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | 常见问题和处理方法 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | 加载器、dyld、系统调用模拟、JIT、AOT |
| [docs/GPU.md](docs/GPU.md) | Metal 桥接的设计与协议 |
| [docs/PERFORMANCE.md](docs/PERFORMANCE.md) | 性能测量与 JIT 优化 |
| [docs/DEBUGGING.md](docs/DEBUGGING.md) | 调试参数、环境变量、差分自测 |
| [docs/OPTIMIZATION.md](docs/OPTIMIZATION.md) | 优化方向与进展 |
| [docs/LAUNCHER-INTEGRATION.md](docs/LAUNCHER-INTEGRATION.md) | 应用包装器的设计 |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | 开发历史与待办 |

## 来源与许可

Attesor 基于 linuxkid473 的 [maclator](https://github.com/linuxkid473/maclator)（MIT），在其基础上新增了循环寄存器分配、离线 AOT 优化、原生 dyld 重定位、应用包装器、调试器、采样 profiler、原生库桥接，以及音频、沙盒、SwiftUI、MetalFX 等支持。

仓库不包含任何苹果的二进制文件。arm64 用户态由你自己从苹果的更新服务器下载，保存在本机。`ref/` 中的苹果开源代码保留原有许可证头（APSL 2.0 / Apache 2.0）。详见 [NOTICE](NOTICE)。

MIT 许可，见 [LICENSE](LICENSE)。
