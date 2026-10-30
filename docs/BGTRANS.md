# 后台直接后继翻译（实验，默认关闭）

精确 token `ATTESOR_JIT_ON=bgtrans` 启用，`ATTESOR_JIT_OFF=bgtrans` 优先。
verify、LOOPRA_BISECT/LOG/ALLOW、loopcensus 诊断模式下禁用；默认不创建 worker。
ON/OFF 原始 token 已参与 AOT `codegen_id`，本次集成版本统一为 62。

## 路径与边界

前台完成一个按需 full 单元后，从实际 `finished_stubs` 取最多两个后继；复制完整
4 KiB 页，只有页内 96 条指令内包含终止指令的候选可入队。一个要求成功设置
BACKGROUND QoS 的 worker 仅使用 owned bytes，生成单基本块 staging，不执行 guest、
不读取 live guest、不分配共享代码缓存、不发布入口。inline/内部回边关闭，iced relocation
记录必须为空；所有 slot continuation 在前台以最终基址重建。

dispatcher / `lookup_or_translate` 非阻塞取结果、try-lock TRANSLATE，再复核 speculative
代际、commpage 模式和完整页内容。发布前登记 slot 与完整页依赖，使用当前 wave-1
`BlockRegistry` / `UnitRec`，在 BLOCKS 锁内复核并发 AOT 插入，不覆盖新入口。
不在 BLOCKS 锁内取得 IMAGES/profile 锁。后台完整结果不加入 tiered 冷计数；冷块本身
没有直接可排队 slot，升级后的 full 块可排队后继。

每次失效在所有 INDEX/no-victim 早退前推进独立 speculative generation；执行 EPOCH
仍负责已发布代码、IBTC 与 continuation。队列/ready 各最多 8，尝试最多 256，单元最多
64 KiB，累计保留 staging 最多 2 MiB；不递归扩散、不等待结果、不无限重试。
`ATTESOR_STATS` 可查看 `bgtrans: adopted=…`，原有前台翻译时间不包括 worker CPU 时间。

## fork 与退出

worker emission / 首次初始化受独立 gate 保护；guest fork 在原有 fork 锁之前暂停新
emission 并等待当前单元离开 gate，可选异步 AOT 保存只尝试 gate，忙则跳过。
父子都释放 gate；子进程先原子停用 bgtrans，不能访问继承 queue/Condvar/OnceLock。
退出停止投机但不 join 低优先级线程；失败 exec 不永久停止原进程 worker。

工作量有界不代表 fork 墙钟延迟有界。VM copy / 内容复核不是与任意并发写入或重映射
原子同步的快照，仍依赖既有 guest IC/ISB 与失效合约；不是全面的映射生命周期修复。

本次已执行 L0、真实 worker/后继采纳、SMC、slot 重定位、fork 及内容复核敏感性测试，
详见 [移植报告](PORT-G2-REPORT.md)。没有性能收益结论，没有 L1/L2 或 GUI 验收，不得默认开启。
