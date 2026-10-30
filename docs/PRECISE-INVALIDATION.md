# 精确 JIT 源依赖失效（#19，默认关闭的实验切片）

```sh
ATTESOR_JIT_ON=preciseinvalidate <attesor> <guest>
# OFF 优先；关闭后使用原失效策略。
ATTESOR_JIT_ON=preciseinvalidate ATTESOR_JIT_OFF=preciseinvalidate <attesor> <guest>
```

## 实际实现

旧 JIT 索引使用公开 label 的最小/最大地址，再给末块增加固定最大长度。
一个短块后面的相邻代码，以及远距离 continuation 之间的空洞，也会落入这个凸包。
新路径在**实际发射期间**记录 reader 接到的所有四字节读取请求，包括 lookahead、
flag/循环分析及失败读取；不靠发射后的重新解码推测生成代码依赖。

每 unit 最多保存 8,192 个不同请求地址，合并相邻/重叠区间；达到上限后退化成
覆盖所有后续读取的保守包络。发生地址加法溢出时按整个地址空间处理。
页索引只登记真实依赖区间经过的 16 KiB guest 页；超宽保守包络登记为 wide unit，
避免枚举巨大地址跨度。正常字节写入/IC IVAU 仅撤销确实相交的 JIT unit。

成功的 BSD `mprotect`、`munmap`、`MAP_FIXED mmap` 仍以覆盖请求的整 guest 页保守
失效；失败的系统调用不撤销代码。修改后的单位仍经过原链槽解绑与 epoch 递增，
没有跳过并发线程 IBTC 的原失效协议，也不回收旧宿主代码。

## AOT 的保守边界

没有依赖证明的旧 `register_block` 仍保留全局保守策略：任意首次失效撤销这类
入口、解绑已追踪链槽，并在本进程余下时间拒绝这类旧入口。

L1 集成发现，这一策略原来连无关的 dyld 数据映射也会撤销主程序全部热 AOT。
现在，仅在 `preciseinvalidate` 下，AOT builder 的 **所有 reader 请求**都限制在
登记的源范围内，包括 lookahead、循环分析和顺序落出边界；范围外交还 dispatcher。
普通 Mach-O 使用 executable segment 的保守凸包（避免 DATA 的权限初始化撤销 TEXT）；
共享 cache 暂用整个登记范围，因此仍可能保守降级。
loader 登记这个完整源范围，而不是从公开入口猜测依赖。相交失效撤销整幅图像；
无关失效保留它。每次失效均保存有界墓碑，阻止首次加载前、并发或迟到的旧 blob
重新发布；超过 1,024 个不相交记录时退化为全地址空间拒绝。旧接口的全局 latch
没有放宽。加载前预检和逐入口发布检查使用原 BLOCKS 锁；没有反转 IMAGES 锁序。
该模式的 loaded 计数只统计实际发布入口，仍须用执行计数证明运行过 AOT。

外部入口来源记录与 BLOCKS 公开映射共用已有锁，不在 AOT 加载持有 IMAGES 时新拿
TRANSLATE/INDEX 锁，不引入新的全局 mutex。无元数据的 AOT 精确失效，必须在未来
加载器提供完整依赖注册后另行实现，本切片不声称完成它。

## 兼容性与未完成项

- 默认关闭，精确逗号 token，OFF 优先。未开启时不新增 map/remap 失效调用，原行为保留。
- AOT 磁盘格式不变；集成的封闭 reader 改变此 opt-in 的生成码，translator version
  从 64 升到 65。既有 ON/OFF 配置 identity 继续隔离实验和默认缓存。
- BSD 层接入不等于覆盖 Mach VM traps、任意 guest/host 直接写入或所有映射 API。
- 当前 guest-text 在翻译期间的可读性/并发修改假设未加强；也不提供正在执行旧代码
  的瞬时抢占、事务式撤销或新的 guest fault 恢复保证。
- 额外源依赖元数据及构建期集合有成本；未证明运行时提速或默认路径开销为零。

integ1 移植已执行 release build；定向 L0 在默认、ON、ON+OFF 三个独立进程
覆盖邻居保留、链槽/epoch、外部入口撤销、请求溢出、BSD 映射成功/失败。
故意不失效的旧代码返回 7 而非新值 42，证明执行测试能够检测该类错误。
`jitcode`、`loops` 自测试各 100 次无差异；日志见 `target/port-g1`。
尚未完成任意并发文本修改、全部 VM API 或默认开启验收。
`--selftest-jit offline` 现在明确隔离每个合成图像生命周期，再从当前字节重新构建、
写盘并加载；不会把 JIT 回退记作 AOT 执行。它另含跨图像边界的真实旧代码执行
对照（42 → 99）及迟到重载拒绝。最终 L1、敏感性和 headless 结果见
[INTEGRATION-REPORT.md](INTEGRATION-REPORT.md)。
