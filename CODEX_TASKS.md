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
