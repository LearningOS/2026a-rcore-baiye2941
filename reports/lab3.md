# 实验三报告：进程管理（ch5）

## 我实现的功能

本次实验完成三部分：把 `sys_get_time`、`sys_mmap`、`sys_munmap` 迁移到本章的 `TaskControlBlock` 进程结构，所有翻译与映射都作用于当前进程的地址空间；实现 `sys_spawn`，复用 `TaskControlBlock::new` 从 ELF 直接建进程并挂好父子关系，避免 `fork + exec` 的全量复制；实现 STRIDE 调度与 `sys_set_priority`，在 TCB 中加入 `stride`/`pass`，调度时选最小 `stride`，让出 CPU 时累加，并按优先级更新 `pass`。最终 `ch5_usertest` 全部通过，`ch5_stride` 的 `max(ratio)/min(ratio)` 约 1.09。

## 具体实现

### 1. 迁移时间与内存映射

#### 1.1 分层

四条系统调用从入口到地址空间，职责分得很清楚：

| 层 | 位置 | 职责 |
| --- | --- | --- |
| 系统调用 | `os/src/syscall/process.rs` | 检查参数的**格式**：对齐、长度、属性位、溢出、上界 |
| 进程管理 | `os/src/task/mod.rs` | 通过 `current_task` 找到当前 Running 的进程 |
| 进程（TCB） | `os/src/task/task.rs` | 提供 `inner_exclusive_access` 访问进程状态 |
| 地址空间 | `os/src/mm/memory_set.rs` | 增删 `MapArea`、改页表、分配/回收物理帧 |

本章和上一章最大的区别是：上一章的内存操作面向全局布局，本章必须通过 `current_task().inner_exclusive_access().memory_set` 拿到**当前进程**的地址空间，否则会改错进程。另外，本章不再维护 `sys_trace`，系统调用分发表里也没有它。

#### 1.2 时间获取

`timer::get_time()` 返回时钟 tick，`CLOCK_FREQ` 是每秒 tick 数，于是：

- `sec = ticks / CLOCK_FREQ`
- `usec = (ticks % CLOCK_FREQ) * 1_000_000 / CLOCK_FREQ`

难点不在算时间，而在**往哪里写**：`ts` 是用户虚拟地址，内核页表里看不到它，直接解引用会写坏内核数据。

- 用 `translated_byte_buffer` 把 `[ts, ts + sizeof(TimeVal))` 按页翻译成若干段内核可访问的可变字节切片；
- 把内核栈上构造好的 `TimeVal` 按字节逐段拷进去。

这样即使 `TimeVal` 的 16 字节被两页劈开，也天然分成两段正确写入。若直接对 `ts` 调 `translated_refmut`，只会翻译起始页，后 8 字节会写到错误的物理页，这是题面 HINT 指出的坑。

#### 1.3 内存映射

`sys_mmap` 的检查顺序和理由如下：

| 检查 | 目的 |
| --- | --- |
| `start % PAGE_SIZE == 0` | 起始地址必须页对齐 |
| `len > 0` | 长度为 0 没有意义 |
| `prot != 0` 且 `prot & !0x7 == 0` | 只允许 R/W/X 三位，且不能为空 |
| `len` 向上取整到页 | 允许非对齐长度，覆盖不足一页的部分 |
| `start.checked_add(len)` | 防止地址回绕 |
| `end <= TRAP_CONTEXT_BASE` | 不能覆盖 TrapContext / Trampoline |
| `MemorySet::is_range_free` | 不能与已有区域重叠 |

几个容易写错的点：

**（1）`len` 是向上取整，不是拒绝。** `ch4_unmap` 里有 `mmap(start - len, len + 1, prot)` 期望成功；若要求 `len` 对齐，这个测例会挂。取整后如果撞上已有区域，由重叠检查返回 -1，这也正好解释了 `ch4_mmap3` 里同样的 `len + 1` 为什么失败。

**（2）`prot` 要一位一位搬，并且强制加用户位。** 用户属性位的第 0/1/2 位对应 R/W/X，而 `MapPermission` 的 R/W/X 在 1/2/3 位，不能整体转换；无论什么属性都要补 `MapPermission::U`，否则用户一访问就缺页。

**（3）`prot = 2` 不要自动补 R。** 只给 W 不给 R 在 RISC-V 里是保留组合，任何访问都会缺页；`ch4_mmap2` 正是靠这一点验证“只写不可读”。若实现里“贴心地”补了 R，测试反而会挂。

**（4）上界检查不能只靠区域列表。** Trampoline 是 `map_trampoline` 直接写页表的，不进入 `areas`；只查 `areas` 拦不住 `TRAP_CONTEXT_BASE` 以上的地址，必须显式加 `end > TRAP_CONTEXT_BASE` 的判断。

**（5）重叠检查在 `MemorySet` 里做。** `insert_framed_area` 的注释写着“Assume no conflicts”，底层 `PageTable::map` 有 `assert!(!pte.is_valid())`。所以 `MemorySet::mmap` 先用 `is_range_free` 扫一遍 `areas`，确认 `[start, end)` 与所有区域都不相交，再插入。

#### 1.4 解除映射

`sys_munmap` 和 `mmap` 的语义不对称：

- `mmap` 的 `len` 向上取整；
- `munmap` 的 `start` 和 `len` 都必须**严格页对齐**，否则返回 -1。

这是因为 `ch4_unmap2` 明确要求 `munmap(start, len + 1)` 和 `munmap(start + 1, len - 1)` 都返回 -1；若 `munmap` 也取整，会误删邻居。

删除时按“精确匹配整块区域”处理：在 `areas` 里找 `vpn_range` 起点和终点都等于请求范围的区域，把它从 `Vec` 里取出，再调用 `MapArea::unmap`。`unmap` 会逐页：

1. 从 `data_frames` 里删掉 `FrameTracker`，触发 `frame_dealloc`，物理页回到回收栈；
2. 清空叶子页表项。

所以 `munmap` 不只是“清页表”，物理页也会自动归还。

### 2. spawn

`TaskControlBlock::new` 已经能“从 ELF 造一个全新进程”：

- `MemorySet::from_elf` 建全新地址空间；
- `pid_alloc` / `kstack_alloc` 分配新 PID 和新内核栈；
- `TaskContext::goto_trap_return` 让任务第一次被调度时直接 `sret` 到新程序的 `_start`；
- `TrapContext::app_init_context` 初始化 `sepc` 并把所有寄存器清零。

它只缺两件事：`parent` 和 `children`。所以 `spawn` 只需要：

1. `Arc::new(TaskControlBlock::new(elf_data))`；
2. 把 `parent` 设为当前进程的 `Weak`；
3. 把自己 push 进父进程的 `children`；
4. 返回 `Arc`，由 `sys_spawn` 取 pid 后 `add_task`。

`sys_spawn` 里先 `getpid` 再 `add_task`，因为 `add_task` 会拿走 `Arc` 的所有权。找不到程序名时返回 -1。

和 `fork + exec` 的区别：

| | `fork` | `exec` | `spawn` |
| --- | --- | --- | --- |
| 地址空间 | 复制父进程全部用户页 | 替换当前进程的地址空间 | 从 ELF 新建 |
| 子进程起点 | 从 `fork` 返回处继续 | 成功时不返回 | 从新程序 `_start` 开始 |
| 开销 | 全量复制 | 丢弃旧空间 | 只建新空间 |

`fork + exec` 会先把父进程地址空间完整复制一遍，再被 `exec` 整个丢掉；`spawn` 直接 `from_elf`，省掉了这次无意义的复制。

另外，`spawn` 不需要像 `sys_fork` 那样写 `trap_cx.x[10] = 0`。`fork` 的子进程从 `ecall` 下一条指令继续，必须用 `a0 = 0` 区分父子；`spawn` 的子进程从 `_start` 开始，`app_init_context` 本来就把寄存器清零了。

### 3. STRIDE 调度与 set_priority

#### 3.1 数据字段

在 `TaskControlBlockInner` 中加入：

- `stride`：已经“跑过”的累计长度；
- `pass`：每运行一个时间片增加的步长，`pass = BIG_STRIDE / priority`。

初始值：`stride = 0`，`pass = BIG_STRIDE / 16`（题目要求初始优先级 16）。`BIG_STRIDE` 取 `1 << 32`：

- 太小会让 `BIG_STRIDE / priority` 的整除误差变大，公平性不达标；
- 太大会让 `stride` 累加接近 `usize` 上限；
- `1 << 32` 在 64 位 `usize` 上误差约十亿分之一，跑几千个时间片也不会溢出。

#### 3.2 调度

把 `TaskManager::fetch` 从 FIFO 改成“选最小 stride”：

- 遍历就绪队列，读每个任务的 `stride`，记录最小值的下标；
- 用 `VecDeque::remove(idx)` 把选中的任务摘出来；
- 队列为空时返回 `None`。

题目提示说测例简单，暴力扫描即可，所以没有用优先队列。

#### 3.3 记账时机

在 `suspend_current_and_run_next` 里，把任务重新放回队列之前执行 `stride += pass`。

选这里是因为它是“任务停止运行”的唯一出口：

- 时钟中断抢占 → 调它；
- `sys_yield` → 调它；
- `sys_read` 阻塞等待 → 调它。

在这里统一记账，不会漏掉任何一种调度路径。放在 `fetch` 里则会在任务还没真正运行时提前收费，时机不对。

#### 3.4 set_priority

```rust
pub fn sys_set_priority(prio: isize) -> isize {
    if prio < 2 {
        return -1;
    }
    let task = current_task().unwrap();
    task.set_priority(prio as usize);
    prio
}
```

- `prio < 2` 直接返回 -1，同时避免 `prio = 0` 时除零；
- 合法时只更新 `pass`，保留 `stride`。换优先级不应该把“已经跑了多少”清零，否则进程可以反复调优先级来洗白欠账；
- `ch5_setprio` 的五个断言（10、`isize::MAX`、0、1、-10）都按预期通过。

#### 3.5 测例结果

`ch5_stride` 六个子进程的优先级为 5 到 10，输出：

| priority | ratio |
| ---: | ---: |
| 5 | 1853120 |
| 6 | 1990000 |
| 7 | 1979142 |
| 8 | 1994600 |
| 9 | 1997955 |
| 10 | 2018560 |

`max(ratio) / min(ratio) = 2018560 / 1853120 ≈ 1.09 < 1.5`，满足公平性要求。

### 已知局限

- **`munmap` 只支持整块删除**：请求范围必须与某个 `MapArea` 完全一致；若把区域切成两半，返回 -1。要支持任意子区间，需要在 `MapArea` 上做拆分。
- **`mmap` 没有物理页预检**：`insert_framed_area` 内部 `frame_alloc().unwrap()`，内存不足时会 panic，而不是返回 -1。本章测例不会触发。
- **`mmap` 不保留用户栈保护页**：保护页只是 `areas` 之外的空洞，若用户显式映射到那段地址，保护页会被填上。
- **`pass` 可能为 0**：`prio` 极大（如 `isize::MAX`）时 `BIG_STRIDE / prio = 0`，该任务 `stride` 几乎不增长，等价于无限高优先级。`ch5_setprio` 只检查返回值，随后进程退出，所以不影响测例。
- **`stride` 没有回绕比较**：本章用 64 位 `usize` 且 `BIG_STRIDE = 1 << 32`，实际不会溢出；8 位溢出的讨论见问答题。
- **阻塞会累积 stride**：`sys_read` 每让出一次 CPU 都会加 `pass`，被阻塞的任务恢复后可能“欠账”较多。这是朴素 STRIDE 的常见问题，本章测例不涉及。

## 问答作业

### 1. 8 位 stride 溢出

`p1.stride = 255`，`p2.stride = 250`，`pass = 10`。

p2 执行一个时间片后：

```text
250 + 10 = 260
260 mod 256 = 4
```

此时 `4 < 255`，调度器仍然选 p2，而不是 p1。

原因：8 位无符号加法在 256 处回绕，回绕后的数值不再保持“谁欠得多”的顺序。朴素的 `<` 比较在回绕点附近会给出错误结果，需要用串行号算术（见第 3 题）。

### 2. 为什么优先级 ≥ 2 时 `STRIDE_MAX - STRIDE_MIN <= BigStride / 2`

设当前 runnable 集合中最大 stride 为 `S_max`，最小为 `S_min`。

达到 `S_max` 的那个进程，上一次被调度时它的 stride 是 `S_max - pass`。那一刻调度器选它，说明它当时是**最小**的，因此：

```text
S_max - pass <= 当时其他所有 runnable 进程的 stride
             <= S_min（stride 只增不减）
```

移项得：

```text
S_max - S_min <= pass = BigStride / priority
```

又因为 `priority >= 2`，所以 `pass <= BigStride / 2`，即：

```text
S_max - S_min <= BigStride / 2
```

直观理解：任何进程最多只能领先当前最小值“一个步长”，而最大步长是 `BigStride / 2`。

### 3. 溢出安全的 `Stride` 比较器

```rust
use core::cmp::Ordering;

struct Stride(u64);

impl PartialOrd for Stride {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let diff = self.0.wrapping_sub(other.0) as i64;
        if diff < 0 {
            Some(Ordering::Less)
        } else {
            Some(Ordering::Greater)
        }
    }
}

impl PartialEq for Stride {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}
```

思路：把无符号差值重新解释为同宽有符号数。差值为负，说明 `self` 在 `other` 的“前半圈”，即 `self < other`。这就是串行号算术。

题目给的 8 位例子（用 `as i8` 验证）：

- `125 - 255 = -130`，`as i8` 回绕成 `126 > 0`，所以 `125 < 255` 为 **false**；
- `129 - 255 = -126`，`as i8` 仍是 `-126 < 0`，所以 `129 < 255` 为 **true**。

这个比较器成立的前提是真实差值小于半个模数。第 2 题已经保证 `STRIDE_MAX - STRIDE_MIN <= BigStride / 2`，而 `BigStride` 取模数的一半（8 位时约 128），所以不会出现“绕半圈以上”的歧义。

## 对本次实验的看法（选填）

- STRIDE 的“最小 stride + 步长”模型很适合教学，暴力扫描就够用，不需要优先队列。
- `mmap` 的 `len` 向上取整与 `munmap` 的严格对齐是一个不对称点，容易写反；测例把这两个边界卡得很清楚。
- `spawn` 复用 `TaskControlBlock::new` 让人直观看到“`fork + exec` 多复制了一遍地址空间”，比单独讲概念更有说服力。
- 建议在题面里进一步明确 `munmap` 是否必须支持部分解除，以及 `mmap` 末页只覆盖几个字节时的判定规则。

## 荣誉准则

1. 在完成本次实验的过程（含此前学习的过程）中，我曾分别与以下各位就（与本次实验相关的）以下方面做过交流，还在代码中对应的位置以注释形式记录了具体的交流对象及内容：

   《你交流的对象说明》

2. 此外，我也参考了以下资料，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：

   《你参考的资料说明》

3. 我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响其评分。

4. 我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。
