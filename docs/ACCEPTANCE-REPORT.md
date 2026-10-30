# integ1 全树 CPU 验收（进行中）

本轮在现有集成工作树移植 #34 policy/autotune 和 integ3 验收工具；不执行 Git 写操作、不修改其他工作树。
旧 integ3 报告和测量仅为方法及历史输入，保存于 `target/acceptance/import/`，不作为本树通过证据。

- 输入 token：`docs/INTEGRATION-REPORT.md` 的 27 项，加重新验收的 `loopra`。
- 所有 token 默认关闭，精确逗号 ON、OFF 优先；#34 L2_ALLOWLIST 为空。
- 翻译器版本统一一次由 65 升至 66，保留集成 Cpu ABI、ISA flags 与缓存身份。
- L1：19 类 × 100000；每 token OFF、ON、全部 ON、全部 ON+本 token OFF。四个循环 seed 各 300000；各环境 debug/release cargo。
- L2：最终默认的全部 19 类 × 300000 × 四 seed，AOT 及 cargo。
- 性能：私有 cache、`--no-aot`、串行交替 OFF/ON 与 ON/OFF，每臂至少 3 次；指定 b/k ARM/x86 二进制、原始 JS 和 headless 启动。
- 仅真实执行且通过计数 oracle 的结果记 PASS；跳过、fallback 和平台限制单列。Metal、网络交由 Claude，不在本轮伪造通过。

实际结果和逐 token 决策将在完整台账结束后填写；当前文档不是最终验收通过证明。
