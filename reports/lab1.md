## 我实现的功能 sys_trace：

在 `os/src/syscall/process.rs` 的 `sys_trace` 函数中使用 `match` 对入参 `trace_request` 进行匹配，根据要求处理 `trace_request` 分别为 `0`、`1`、`2`、`_` 的情况，其中 `2` 是要查询当前任务调用编号为 `id` 的系统调用的次数，具体实现如下：

1. 在 `os/src/task/task.rs` 为 `TaskControlBlock` 结构体添加了一个 `syscall_count` 字段，用来统计系统调用次数，具体类型为 `[u16; 512]`（因为 `os/src/syscall/mod.rs` 里的系统调用号最大为 410，所以使用了 512，如果不够后续可改。`lazy_static` 初始化时 `TaskControlBlock` 数组需要先在栈上整体构造再移入 static，而 `entry.asm` 中的启动栈只有 64 KiB。若用 `[usize; 1024]`，单个 `TaskControlBlock` 中该数组为 1024 × 8 B = 8 KiB，乘以 16 个任务后约 130 KiB，会溢出；改用 `[u16; 512]` 后为 512 × 2 B = 1 KiB，16 个共约 16 KiB。即使后续把长度扩到 1024，单个也只有 1024 × 2 B = 2 KiB，总量约 32 KiB，仍远小于 64 KiB，具有扩展余地）。

2. 在 `os/src/task/mod.rs` 为 `TaskManager` 添加了 `increase_syscall_count` 和 `get_syscall_count` 方法，分别用于增加系统调用次数和获取系统调用次数，并通过模块级 `pub fn` 暴露给 `syscall` 模块；方法用 `inner.exclusive_access()` 取得 `TaskManagerInner` 的可变借用，以 `inner.current_task` 定位当前任务，因此统计天然是「每任务独立」的。下标用 `get_mut()` 获取，编号越界时静默跳过；自增用 `saturating_add(1)`，避免 u16 溢出回绕。`get_syscall_count` 则使用 `get().copied().unwrap_or(0)` 取值，编号越界时返回 0 而不是 panic。

3. 计数写在 `os/src/syscall/mod.rs` 的 `syscall` 系统调用总入口，在 `match` 之前统一执行，这样所有的系统调用都会被统计。查询编号 410 时，本次调用也会先被统计。

## 简答作业：

1. 使用的 SBI 为 RustSBI 0.4.1，在 `ch3` 分支运行 `make -C ci-user test CHAPTER=3`，内核日志在 `Hello, world!` 之后依次出现：

    ```
    [kernel] PageFault in application, bad addr = 0x0, bad instruction = 0x804003a4, kernel killed it.
    [kernel] IllegalInstruction in application, kernel killed it.
    [kernel] IllegalInstruction in application, kernel killed it.
    ```

    三个测例分别验证了 U 态程序的三种非法行为，内核都会在 trap_handler 中捕获并终止应用。

    ch3b_bad_address（Store Access Fault，日志统一打印为 "PageFault"）：应用向非法地址 0x0 写入数据。由于 satp 处于 Bare 模式，该访问不会被地址翻译拦截，而是触发 Store/AMO Access Fault（scause=7）：QEMU virt 平台的 RAM 从 0x80000000 开始，0x0 附近没有可访问的物理内存/设备，且 RustSBI 固件还配置了 PMP 物理内存保护。内核 trap_handler 中将 StoreFault/StorePageFault 两个分支合并处理并打印 "PageFault" 字样，bad addr = 0x0 是 stval（出错地址），bad instruction = 0x804003a4 是 cx.sepc（出错指令地址），随后杀掉该应用。

    ch3b_bad_register（IllegalInstruction）：应用试图在 U 态读取 S 态 CSR（如 csrr 读取 sstatus）。CSR 访问权限由特权级机制保证：U 态访问 S 态 CSR 属于特权操作，CPU 触发 IllegalInstruction 异常（scause=2），内核捕获后终止应用。

    ch3b_bad_instructions（IllegalInstruction）：应用执行 S 态特权指令（如 sret），U 态执行同样触发 IllegalInstruction 异常，被内核终止。

    结论：ch3 的用户态/内核态隔离不靠页表，而是靠 (1) RISC-V 特权级机制（U 态不能执行 S 态指令、不能访问 S 态 CSR）；(2) 物理地址保护（对 0x0 等无映射区域的访问触发 access fault）。真正的基于页表的内存隔离要到 ch4 引入 SV39 分页后才建立。

2. （1）刚进入 `__restore` 时，sp 代表了什么值。请指出 `__restore` 的两种使用情景。

    答：

    刚进入 `__restore` 时，sp 代表内核栈上刚分配好的 TrapContext 的起始地址（栈顶向低地址增长了 34*8 字节），其中保存了全部通用寄存器和 sstatus/sepc/用户栈指针。

    `__restore` 的两种使用情景：

    - 情景一（正常 trap 返回）：U 态程序触发 trap（如 ecall）进入 S 态，`__alltraps` 保存现场 → trap_handler 处理 → 返回后从 `__alltraps` 尾部落入 `__restore`，恢复被打断的应用上下文，sret 回到 U 态。

    - 情景二（首次启动一个应用/任务切换）：内核为一个新任务在其内核栈顶预先构造好一份初始 TrapContext（含应用入口地址、用户栈指针、U 态的 sstatus），然后把 sp 指向这份 TrapContext 后直接 call `__restore`，从而“伪造一次 trap 返回”，让 CPU 从不存在的旧上下文直接切入新应用的第一条指令。`__switch` 切换内核栈后同样是跳到 `__restore` 完成进入应用的过程。

    （2）L43-L48：这几行汇编代码特殊处理了哪些寄存器？这些寄存器的值对于进入用户态有何意义？请分别解释。

    答：

    特殊处理了三个 S 态 CSR：sstatus、sepc、sscratch。前三行先用 t0/t1/t2（x5-x7，可放心使用，因为它们的旧值已保存在 TrapContext 中）从 TrapContext 取出三个值，再用 csrw 特权指令写回对应 CSR；必须放在恢复通用寄存器之前执行。sstatus 的 SPP 记录了陷入 Trap 前的特权级，sepc 记录了 Trap 发生时被打断的指令地址，sscratch 写入用户栈指针。这些都是为了后续的 sret 返回 U 态时恢复应用上下文所必须的，其中 sscratch 换回内核栈后，会继续等下一次 Trap。

    （3）L50-L56：为何跳过了 x2 和 x4？

    答：x2 保存的是内核栈栈顶指针 sp，循环里的 `ld x*, offset(sp)` 全以 sp 为基址，若在循环中恢复 x2，后续所有寻址立即失效。x4 是线程指针，ch3 是单线程裸机程序，用不到所以不用管。

    （4）L60：该指令之后，sp 和 sscratch 中的值分别有什么意义？

    答：该指令之后，sp 和 sscratch 中的值分别是应用的用户栈指针和内核栈栈顶指针。其中 sscratch 会继续等待下一次 Trap。

    （5）`__restore` 中发生状态切换在哪一条指令？为何该指令执行之后会进入用户态？

    答：状态切换发生在 L61 的 sret。它执行前，L46-L47 已把 sstatus（SPP=0，即陷入前处于 U 态）和 sepc（应用的返回地址/入口地址）写入对应 CSR。sret 执行时硬件读取 SPP=0，将特权级从 S 切换到 U，并把 PC 置为 sepc，于是 CPU 以用户态从应用处继续执行。此前的 csrrw 只完成栈指针交换，不涉及特权级；因此状态切换由 sret 这一条指令触发，而切换能否进入用户态则取决于前面恢复的 CSR 值。

    （6）L13：该指令之后，sp 和 sscratch 中的值分别有什么意义？

    答：该指令之后，sp 和 sscratch 中的值分别是内核栈栈顶指针和应用的用户栈指针。两个值会在 `__restore` 中被恢复。

    （7）从 U 态进入 S 态是哪一条指令发生的？

    答：ecall 指令。

## 荣誉准则：

1. 在完成本次实验的过程（含此前学习的过程）中，我曾分别与以下各位就（与本次实验相关的）以下方面做过交流，还在代码中对应的位置以注释形式记录了具体的交流对象及内容：

    《你交流的对象说明》

2. 此外，我也参考了以下资料，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：

    《你参考的资料说明》

3. 我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响其评分。

4. 我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。
