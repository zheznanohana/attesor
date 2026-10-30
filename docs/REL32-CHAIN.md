# rel32chain：不可变 rel32 边与静止期 data-slot 发布

`ATTESOR_JIT_ON=rel32chain` 精确 opt-in，OFF 优先。默认不开启。
不修改已发布的指令，不以五字节写入或对齐原子写入冒充安全的指令热补丁。

## 两类入口、两类边

- 发布前在 assembler Vec 中把 `FF25` 六字节跳转改成 `E9 disp32; NOP`。
  只指向较早发布、有永久 guard、非 cold 的运行时 unit；形状、范围或生命周期不满足时保留 slot。
- 每个这样的 unit 有两个入口：`guard_host` 是放在 unit 尾部的永久 alive/epoch guard；
  `host` 是 canonical body。不可变 rel32 指向前者；dispatcher、线程 IBTC 和可撤销的 mutable slot 指向后者。
  普通路径沿用 OFF 的 canonical-entry/invalidation 合约，不反复支付只为不可变旧边所需的 guard。
  guard 附录不改变同一 unit 的 hot body、loop、slot、constant pool 字节和偏移；定向测试逐字节比较。
  它仍增加整个 unit 的长度，因此不保证后续 unit 的全局地址/缓存布局与 OFF 相同。
- immutable incoming 失效后不能改写其指令，只撤销永久 alive cell。代码和 cell 永不释放/复用。
  guard 同时检查 alive 与 entry/global epoch，拒绝时恢复目标 guest PC、清空 EXIT_DATA 并返回 dispatcher。
- mutable incoming 使用已有 incoming 索引，目标失效时恢复 dispatcher continuation；禁止指向 cold 入口。
  数据槽原子 Release 发布，不写任何已发布指令。

## 为什么晚到目标现在可以链接

原实现永久禁止运行期 slot 链接，因此首次翻译时尚未存在的调用目标每次都退回 dispatcher + CPUID。
FIX2 用进程内的 **graph lease** 保证发布后的取指序列化：

1. 每次进入 JIT，先取得 graph read lease，再执行带 compiler fences 的 CPUID，最后进入机器码。
   lease 保持到完整 JIT 执行返回，包括所有跨 unit 边、guest host-call 帧和 helper。
2. 新 data-slot 边先尝试取得 graph **exclusive** lease，再取得 TRANSLATE、确认当前 target/slot/cold 状态并发布。
   使用 `try_write`，有任何执行者时立即保留 fallback，不等待无限循环 guest。
3. 因而新边发布时不存在正在执行旧 graph 的线程；之后每个执行者都先取得 read lease 并 CPUID。
   执行期间不能再加入通向新代码的数据边。已有 immutable rel32 边只指向旧的已发布 guarded graph。
4. 失效只撤销数据与 guard、缩减图，不引入新代码。它不取得 exclusive lease，故执行中的 helper 可以失效而不自锁。
   取得 graph exclusive lease 必须在 TRANSLATE 之前；entry 不带 INDEX/BLOCKS/TRANSLATE 锁。

这是实现层面的协议推导，不把 RwLock 的数据 Acquire 当作取指屏障；实际取指屏障仍是进入机器码前的 CPUID。
旧的“仅按发布时间构成 DAG”现在只描述 immutable rel32 子图，而非整个 mutable 运行图。

## fork 与边界

图锁通过永久分配的指针访问。现有 `fork_safely` child hook 中换成新空锁，
避免其他已消失线程的 inherited read lease 使子进程无法发布；不释放旧锁，不在父进程修改它。
定向测试在另一个线程持 read lease 时执行真实 guest-fork helper，验证子进程可以取得 exclusive lease。
这不扩展原工程对任意多线程 guest fork、信号重入或并发非协作 SMC 的保证。

没有热 rel32 patch、旧源回填、AOT direct link、超范围跳板或代码回收。
忙的多线程 graph 可能一直保留 dispatcher fallback；本次性能验收是给定单线程 kernels，不承诺所有多线程程序提速。
测试覆盖实际 E9 位移/guard、跨线程发布、dead/alive-epoch 拒绝、晚到 call/native-return、静止期发布、fork 和源码敏感性。
数据与最终测量见 `FIX2-REPORT.md`。
