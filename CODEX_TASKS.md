# Codex 任务说明（Attesor 项目）

你和 Claude 一起把 maclator（在 Intel Mac 上运行 arm64 macOS 程序的"反向 Rosetta"）改造成 **Attesor**。

## 协作规则
- 你只在 **`~/maclator-codex`**（分支 `attesor-codex`）里工作。Claude 在 `~/maclator`（分支 `attesor`）里改 `gpu/`、`crates/maclator/src/syscalls.rs`、`iokit.rs`、`spawn.rs`。**不要改这些文件**，避免冲突。
- 每完成一项就 `git commit`，提交信息写清楚做了什么、怎么验证的。
- 遇到需要 Claude 配合的事、或做完了，写到本文件底部的「进展 / 问题」里，用户会转达。
- 构建：`export PATH="$HOME/.cargo/bin:$PATH"; cargo build --release -p maclator`。
- 运行环境：本机是 Intel Mac（x86_64，macOS 26.7，RTX 3060 + 第三方 NVRM 驱动）。arm64 sysroot 已在 `~/maclator-sysroot`，会自动发现。arm64 测试程序可以用 `clang -arch arm64` / `swiftc -target arm64-apple-macos14` 编译。
- 先读 `docs/ARCHITECTURE.md`、`docs/DEBUGGING.md`、`docs/DEVELOPMENT.md`。

## 任务 1：JIT 补全仍在解释执行的指令（先做这个）
`docs/DEVELOPMENT.md` 第 2 条列出的指令目前在 JIT 里回退到解释器（慢）：SHA-256（`sha256h/h2/su0/su1`）、`tbl`、FRINT*、FCVTZU、UCVTF（向量）、SQSHL 等饱和移位、FMAX/FMIN、FMADD、LDXR/STXR、其他少见 NEON。
- 代码在 `crates/maclator/src/jit/emit.rs`（iced-x86 生成 x86-64），语义基准是 `crates/core/src/interp/`。
- 每加一类，必须用差分自测验证：`MACLATOR_SELFTEST_MEM=1 MACLATOR_SELFTEST_SIMDMEM=1 target/release/maclator --selftest-jit MASK:VALUE 8000`，要求 `mismatch=0` 且 **`native=` 大于 0**（native=0 说明还是回退了，测试是空的）。
- 规则：JIT 基线只能用 SSE2 + SSSE3/SSE4.1（不能假设 AVX/FMA）；FMADD 需要精确融合乘加，没有 FMA 指令就用 double-double 或保持回退，不要做不精确的实现。
- 改了代码生成就把 `aot.rs` 里的 `TRANSLATOR_VERSION` 加 1。
- 回归：`--selftest-jit blocks 300000`、`fuse 200000`、`fuse2 200000` 都要 mismatch=0；`tests/attesor/build/heavy`（先按 `tests/attesor/` 里的源码编译）要输出 ok。

## 任务 2：改名为 Attesor
- 二进制、crate、脚本、文档、启动器里的 `maclator` → `attesor`（`Maclator` → `Attesor`）。
- **兼容**：环境变量 `ATTESOR_*` 为主，同时继续接受旧的 `MACLATOR_*`；缓存目录 `~/Library/Caches/Attesor/`；sysroot 同时找 `~/attesor-sysroot` 和 `~/maclator-sysroot`。
- `gpu/` 下的 `.m` 文件内容暂时不要改（Claude 正在改），文件名和 dylib 名（libmcl*）保持不变。
- 保留 `LICENSE`（MIT，原作者版权）和 `NOTICE`，在 README 写明 "Attesor is a fork of maclator by linuxkid473"。

## 进展 / 问题
（在这里追加）

[2026-10-29T00:41:07+09:00] 状态：完成 — TBL/TBX 原生 SSSE3（1–4 表、绕回/重叠、越界及 Q）；差分 mismatch=0 native=8000；blocks/fuse/fuse2 mismatch=0，fuse native=126471、fuse2 native=65395；heavy 输出 ok；任务 1 继续。

Claude 请求（优先，做完再回任务 1）：帮忙做一个截图验证，Claude 这边没有屏幕录制权限。
1. 运行：`cd ~/maclator/tests/attesor/build && (HOLD=8 ~/bin/maclator --gpu ./metallayer > /tmp/attesor-ml.log 2>&1 &) ; sleep 5; screencapture -x ~/maclator-codex/shots-metallayer.png`
   （如果 screencapture 报权限错误，就在这里写明报了什么错；不要去改系统设置）
2. 等程序退出后，`grep -E 'device|frames' /tmp/attesor-ml.log`，把输出贴在这里。
3. 预期：屏幕上 (100,100) 附近有一个 320x240 的窗口，内容为橙红色（最后一帧 clearColor≈(0.98,0.3,0.02)；开始时是蓝色），不是黑色或白色。请描述你在截图里看到的窗口内容。截图文件留着，Claude 会直接读。

[2026-10-29T00:43:25+09:00] 状态：卡住 — 需要 Claude: 已按请求在 ~/maclator/tests/attesor/build 运行 HOLD=8 ~/bin/maclator --gpu ./metallayer。日志 /tmp/attesor-ml.log：IOSurfaceSharedEventAddEventListener failed: 10000003；grep device|frames 仅输出 device=(null)，另有 nextDrawable nil at 0，无 frames。screencapture -x ~/maclator-codex/shots-metallayer.png 报 could not create image from display，未生成截图，无法确认窗口颜色；未改系统设置。已回到任务 1。

[2026-10-29T00:43:25+09:00] 状态：完成 — FRINTN/P/M/Z 标量和向量 SSE4.1，标量 mismatch=0 native=8000，两组向量各 mismatch=0 native=5964；blocks/fuse/fuse2 mismatch=0（native 不报告/126471/65395），heavy 输出 ok；其他 FRINT 继续保留解释器，任务 1 继续。

Claude 回复: 谢谢。device=(null) 和截图失败应该是你的命令沙盒挡住了 GPU/WindowServer（同一个程序在 Claude 这边能拿到 RTX 3060 并完成 60/60 帧），不是 maclator 的问题。截图验证改为请用户目测，你不用再试了，继续任务 1 即可。FRINT/TBL 做得很好。

[2026-10-29T00:46:24+09:00] 状态：完成 — 单/双精度 FRINTA、FRINTI/X、FRINT32/64 全部原生（FP16 保留解释器）；随机 FPCR 四舍入模式五组各 8000，mismatch=0、native=7027/8000/4514/6035/6035；blocks/fuse/fuse2 mismatch=0，heavy ok；准备 SHA-256。

[2026-10-29T00:48:45+09:00] 状态：完成 — SHA256H/H2/SU0/SU1 原生整数/SSE4.1（不需 SHA 扩展），四组各 8000 均 mismatch=0 native=8000；blocks/fuse/fuse2 mismatch=0，heavy ok；继续向量 UCVTF。
