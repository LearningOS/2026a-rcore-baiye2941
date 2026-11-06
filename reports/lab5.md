# 实验五报告：并发与死锁检测（ch8）

## 我实现的功能

本次实验为 mutex 与 semaphore 加入了基于银行家算法的死锁检测：收集进程内所有存活线程与各资源的占用/等待关系，构造 Available、Allocation、Need 三个矩阵做安全性检查，若本次请求会导致不安全状态则返回 `-0xDEAD` 拒绝。

为此给 `Mutex`、`Semaphore` 各加了只读快照接口（`MutexBlocking` 记录持有者 tid，`Semaphore` 按线程记账持有量），并完成 `sys_enable_deadlock_detect` 的开关、参数校验与 fork 继承。

## 具体实现

### 1. 从资源对象里“探”出状态

死锁检测需要对“此刻谁占着哪个资源、谁在等哪个资源”做全局快照。三类对象各自暴露一个只读快照结构，全部在锁内复制出标量/小向量后立刻放锁，不在持锁状态下跑算法：

| 快照 | 位置 | 字段 | 说明 |
| --- | --- | --- | --- |
| `MutexProbe` | `os/src/sync/mutex.rs` | `locked`、`owner: Option<usize>`、`waiters: Vec<usize>` | 等待队列里存的是 `Arc<TaskControlBlock>`，快照时用 `tid_of` 换成 tid，避免算法层再碰 TCB 锁 |
| `SemProbe` | `os/src/sync/semaphore.rs` | `count: isize`、`waiters`、`holders: Vec<(usize, usize)>` | `count` 允许为负（等待者已预支）；`holders` 记录每个线程持有的单位数 |
| 无 | `MutexSpin::probe` | 返回 `None` | 自旋锁不阻塞，不构成死锁环路，直接跳过 |

`Mutex` trait 因此新增 `fn probe(&self) -> Option<MutexProbe>`，`MutexBlockingInner` 新增 `owner` 字段：

- `lock` 成功时 `owner = Some(current_tid())`；
- `unlock` 若等待队列非空，直接把 `owner` 改成被唤醒线程的 tid 并 `wakeup_task`，`locked` 保持 true（**直接移交**）；队列空才把 `locked` 置 false、`owner` 清空。

选直接移交而不是“释放后竞争”，是因为检测需要一个良定义的持有者：移交语义下 `locked == true` 与 `owner.is_some()` 始终同进同退，检测器看到的快照不会出现“锁空着但没人负责”的中间态。本内核的 `block_current_and_run_next` 只在被 `add_task` 之后才会返回，不存在伪唤醒，所以不需要 `loop` 重试。

### 2. 银行家安全算法

`os/src/task/deadlock.rs` 的核心是 `is_safe(available, allocation, need)`，完全按指导书的四步：

1. `work = available`，`finish[0..n] = false`；
2. 找 `finish[i] == false` 且对所有 j 满足 `need[i][j] <= work[j]` 的线程；
3. 找到就 `work[j] += allocation[i][j]`、`finish[i] = true`，回到步骤 2；
4. 全部 `finish` 为真则安全，否则不安全。

两个工程细节：

- 矩阵元素用 `isize` 而不是 `usize`：信号量的 `count` 可能为负，容量差值不能回绕；
- 用一个 `loop` + `progressed` 标志反复扫描，而不是只走一遍：一个线程完成后释放的资源可能让多个先前不可满足的线程依次通过，单趟扫描会漏判。

`tid` 到矩阵行号的映射由 `alive_tids` 提供：遍历 `process_inner.tasks`，只保留 `res.is_some()`（尚未退出）的线程，枚举下标即 tid。已退出线程不在矩阵里，因此它“占着没还”的资源自然无人归还，算法会判不安全——这正是我们想要的语义。

### 3. mutex 检测：把待请求的资源补进 Need

`check_mutex_deadlock(process, pending_mutex)` 的做法是把“每把 mutex”当成一类只有 1 个单位的资源：

| 矩阵 | 取值 |
| --- | --- |
| `available[j]` | 未被占用则 1，被占用则 0 |
| `allocation[owner][j]` | 持有者为 1（`owner` 已不在 `alive_tids` 里时找不到对应行，判不安全） |
| `need[waiter][j]` | 等待队列中的线程各为 1 |
| `need[current][pending]` | 当前线程本次请求为 1 |

最后一项是关键：调用检测时，当前线程**还没有进入等待队列**（先检测、后被拒绝或阻塞），如果只按快照建矩阵，会出现“我要求的东西没算进我的 Need”，于是自己申请自己已持有的锁（自死锁）反而被判安全。把 `pending_mutex` 显式补进 Need 后，`ch8_deadlock_mutex1` 里同一线程二次加锁就能被正确识别：`available = 0`、自己的 `allocation = 1`、自己的 `need = 1`，无人能完成，判不安全。

### 4. semaphore 检测：多单位资源与持有者账本

semaphore 与 mutex 的差别是“一类资源有多个单位、可以被多个线程分持”，所以 `allocation` 不再是非 0/1，`available` 也不止 0/1：

| 矩阵 | 取值 |
| --- | --- |
| `available[j]` | `count.max(0)`：负值表示已有等待者预支，可利用量按 0 算 |
| `allocation[tid][j]` | 该线程在 `holders` 里记录的单位数，可能大于 1 |
| `need[waiter][j]` | 等待队列线程各为 1 |
| `need[current][pending]` | 当前线程本次请求为 1 |

难点在于“谁持有了多少”内核本来并不记录，因此给 `SemaphoreInner` 加了 `holders: Vec<(tid, usize)>` 并在三处维护：

- `down` 立即成功时 `record_hold(current_tid)`；
- `up` 时 `release_hold(current_tid)`：优先扣自己的单位，自己没有则扣任意一个持有者（信号量单位是匿名的，`up` 并不要求“还自己借的那个”）；
- `up` 唤醒等待者时，被唤醒者实际上完成了一次 `down`，因此对它 `record_hold`。

这种记账对“单位匿名”的信号量是近似：A 借的单位可能被 B 归还，矩阵里会记到 B 头上。但银行家算法只关心“总量与每个线程的待满足量”，只要每类资源的总分配量与实际 `count` 变化一致（我们的记账严格满足这一点），安全性判定的结论就不受影响。

### 5. 检测时机、返回值与开关

| 位置 | 触发条件 | 动作 |
| --- | --- | --- |
| `sys_mutex_lock` | `deadlock_detect` 且目标锁 `probe().locked`（本次会阻塞） | 不安全则直接返回 `-0xDEAD`，不阻塞 |
| `sys_semaphore_down` | `deadlock_detect` 且 `probe().count <= 0`（本次会阻塞） | 不安全则直接返回 `-0xDEAD`，不阻塞 |
| `sys_enable_deadlock_detect` | 参数 `enabled > 1` | 返回 -1；否则 `deadlock_detect = enabled == 1`，返回 0 |

只在实际会阻塞时才跑算法：一是省掉绝大多数无需检测的请求（`ch8b_*` 的正常加锁路径几乎不受影响），二是避免把“本来立刻能拿到的资源”误判成死锁。

检测结果是“拒绝这次请求”，而不是“杀掉某个线程”：调用者拿到 `-0xDEAD` 后如何恢复由用户程序决定（`ch8_deadlock_sem1` 里就是回滚已持有的资源并 `exit(-1)`），这比内核替用户选一个牺牲者更安全。

开关状态放在 `ProcessControlBlockInner` 里，`ProcessControlBlock::new` 初始化为 false，`fork` 时复制父进程的值——死锁的本质是进程内部线程之间的资源环路，所以这个开关天然是进程级的。

### 6. 附带修复：sys_get_time

ch8 分支的 `os/src/syscall/process.rs` 里 `sys_get_time` 仍是返回 `-1` 的桩函数。而用户库的 `sleep()` 是忙等实现：

- `get_time()` 在 `sys_get_time` 返回非 0 时返回 `-1`；
- 于是 `while get_time() < start + period` 恒为真，主线程永远在 `sys_yield` 里打转。

`ch8_deadlock_sem1/sem2` 的主线程正是在 `sleep(500)` / `sleep(1000)` 之后才 `semaphore_up(SEM_BARRIER)`，因此两个测例会永久挂起（子线程全卡在 barrier 上，日志里 `alloc[0]` 一直是 main 持有的那几份单位）。

按 lab2 的思路补回实现：先取 `get_time()` 的 ticks 换算成 `TimeVal { sec, usec }`，再用 `translated_byte_buffer(token, ts, size_of::<TimeVal>())` 拿到目标用户页对应的内核可写切片，逐段 `copy_from_slice`。用 `translated_byte_buffer` 而不是 `translated_refmut`，是因为 `TimeVal` 恰好可能跨页，前者会把连续的虚拟区间拆成多段物理缓冲区，跨页也能写全。


## 问答题

### 1. 主线程（0 号线程）退出时，需要回收哪些资源？其他线程的 TCB 可能被哪些位置引用，是否需要回收，为什么？

**需要回收的资源**（对应 `os/src/task/mod.rs` 的 `exit_current_and_run_next` 中 `tid == 0` 分支）：

| 资源 | 回收方式 |
| --- | --- |
| pid → PCB 映射 | `remove_from_pid2process(pid)`，否则后续 `waitpid`/`pid2process` 会拿到已析构的 PCB |
| 进程状态 | 置 `is_zombie = true`、写 `exit_code`，交给父进程 `waitpid`；同时把 `children` 全部改挂到 `INITPROC` |
| 各线程的 `TaskUserRes`（tid、用户栈、TrapContext） | 逐线程 `res.take()` 收集到 `recycle_res`，**先放掉 `process_inner` 再统一释放**，避免释放时重入 PCB 锁 |
| 用户地址空间 | `memory_set.recycle_data_pages()` 归还数据页帧；页表本身随 `MemorySet` 的 drop 释放 |
| 打开的文件 | `fd_table.clear()`，`Arc<dyn File>` 计数归零后关闭 inode |
| 进程内的 mutex / semaphore / condvar | 随 PCB 一起 drop；其中的等待队列持有 TCB 的 `Arc`，也随之一并释放 |
| TCB 本身（含内核栈） | `tasks.clear()`，最后一个 `Arc` 释放时 `TaskControlBlock` 析构，内核栈 `KernelStack` 归还 |

**TCB 可能被引用的位置与是否需要主动回收**：

| 位置 | 是否需回收 | 原因 |
| --- | --- | --- |
| 全局调度器就绪队列（`TaskManager`） | **需要** | 不清掉的话，已退出线程还会被调度，恢复现场时访问的页表/用户栈已经归还，属于悬挂指针；这也是 `remove_inactive_task` 的职责之一 |
| 定时器队列（`sleep` 中的线程） | **需要** | 否则到期时 `check_timer` 会 `wakeup_task`，把死任务重新塞回就绪队列 |
| CPU 当前任务 / idle | 不需要额外处理 | 正在执行退出的就是它本身，随后 `schedule` 换出，不会再回到用户态 |
| mutex / semaphore / condvar 的等待队列 | 不需要逐个回收 | 题目约定同步原语只在单进程内使用，这些对象挂在 PCB 上，进程销毁时一起释放；但要注意它们持有 TCB 的 `Arc`，所以必须先让 PCB 可析构，TCB 才会真正释放 |
| `process_inner.tasks` | **需要** | 它本身就是 TCB 的持有者，`tasks.clear()` 是触发析构的最后一步 |
| 父进程的 `children` | 进程粒度，与线程无关 | `children` 存的是子进程 PCB，不是线程 TCB；线程不参与 `fork`/`waitpid` 的父子关系 |
| 其他线程自己的 `parent` / 信号等 | 不需要 | 都是弱引用或计数归零后自然释放 |

一句话：**凡是能让已退出线程“再被调度或再被唤醒”的引用都必须主动摘除（调度器、定时器），其余只要保证 PCB 能按序析构（先回收用户资源、再清空任务表），TCB 就会随 `Arc` 计数归零自然释放。**

### 2. 对比 Mutex1 与 Mutex2 两种实现的区别，以及可能导致的问题

| 比较项 | Mutex1 | Mutex2 |
| --- | --- | --- |
| `lock` 结构 | `loop`：被唤醒后**重新检查** `locked` | 不循环：检查一次，`locked` 为真就入队阻塞 |
| `unlock` 顺序 | 先 `locked = false`，再唤醒队首 | 队列非空时**不**置 `false`，直接把锁“移交”给队首；队列空才置 `false` |
| 唤醒语义 | 释放后竞争：唤醒只是让线程回到就绪队列，锁处于空闲态，谁先跑谁拿 | 直接移交（handoff）：被唤醒者醒来即认为持有锁 |
| 公平性 | 新来的线程可能与刚被唤醒的线程抢锁，被唤醒者可能再次失败（饥饿） | 严格 FIFO 移交，不会插队 |
| 正确性依赖 | 只依赖 `locked` 的检查，容忍伪唤醒 | 依赖“被唤醒 == 已持锁”的严格配对，不容忍任何其它唤醒源 |

**Mutex1 的问题**：安全性好但效率与公平性差。`unlock` 之后锁处于空闲态，可能出现“唤醒 A 之后 B 抢先拿到锁”，A 醒来发现锁又被占、继续阻塞——一次 `unlock` 可能引发多次上下文切换。极端情况下某个线程一直抢不到，形成饥饿。此外 `loop` 在 `UPSafeCell` 上反复拿放锁，在多核下会带来无谓的缓存行争用。

**Mutex2 的问题**：把“唤醒”与“获得锁”绑定，任何破坏这个绑定的情况都会出错：

- **伪唤醒 / 其它唤醒源**：若线程因信号、定时器或同一 TCB 被 `add_task` 两次而醒来，它不会重新检查就直接进入临界区，此时 `locked` 可能仍为 true 且持有者是别人，互斥被破坏（两个线程同时进临界区）；
- **持有者异常退出**：锁已经“移交给”队首线程，如果该线程在进入临界区前 `exit`，`locked` 永远不会被清，队列里其它线程永久阻塞；
- **移交对象已死**：若等待队列里残留已退出线程，`unlock` 会把锁移交给它，锁同样丢失（本内核 `exit` 时会从等待队列摘除，但这是额外的约束，不是 `Mutex2` 自身保证的）。

**本实验的选择**：`MutexBlocking` 采用 Mutex2 的直接移交语义，原因是死锁检测需要一个确定的 `owner`——移交语义下 `locked` 与 `owner` 始终一致，`probe()` 拿到的快照不会出现“锁被持有但不知道持有者”的窗口，否则 `allocation` 行无法定位，检测结果会不可靠。作为补偿，内核里唯一的阻塞原语 `block_current_and_run_next` 只在该任务被显式 `add_task` 后才会返回，不会出现伪唤醒，因此不需要 Mutex1 的重试循环。

## 对本次实验的看法（选填）

- 题面把“可能发生死锁”定义为“安全性检查失败”很关键：它要求的不是“检测到已经死锁的环路”，而是“这次分配之后是否还存在安全序列”，所以哪怕此刻没有成环，也可能被拒绝。理解这一点后 `pending` 必须计入 Need 才显得自然。
- 最容易踩的坑是“检测时机”：如果在 `lock` 内部、真正阻塞之前不建模本次请求，自死锁永远检不出来（`ch8_deadlock_mutex1` 就是专门卡这个点的）。
- 信号量的难度明显高于 mutex，因为内核原本不记录“谁持有多少单位”。加 `holders` 账本时要注意三点：`up` 唤醒等待者等价于对方完成一次 `down`、`count` 可能为负、单位匿名导致归还的对象未必是借出的对象。
- 工作量与题面估计的 100 行相当，难点不在算法（银行家算法本身很简单），而在“快照要一致、锁不能重入、检测要发生在正确的位置”这三件事上。
- 一个小建议：题面可以明确“检测到死锁后是否允许回滚已持有的资源”，现在完全由用户程序自己处理，容易写出中途 `exit` 导致资源永久占用的用例。

## 荣誉准则

1. 在完成本次实验的过程（含此前学习的过程）中，我曾分别与以下各位就（与本次实验相关的）以下方面做过交流，还在代码中对应的位置以注释形式记录了具体的交流对象及内容：

   （无）

2. 此外，我也参考了以下资料，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：

   rCore-Tutorial-Guide 第八章《并发》及其练习页（银行家算法与 `sys_enable_deadlock_detect` 的接口约定）。

3. 我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响其评分。

4. 我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。
