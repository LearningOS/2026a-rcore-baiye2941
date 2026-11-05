# 实验四报告：文件系统与 I/O 重定向（ch6）

## 我实现的功能

本次实验在 easy-fs 上实现硬链接：为磁盘 inode 增加链接计数 `nlink`，实现 `linkat`、`unlinkat`、`fstat` 三个系统调用。`linkat` 在目录中追加一个指向已有 inode 的目录项并把 `nlink` 加一，同名返回 -1；`unlinkat` 用全零目录项覆盖原目录项（墓碑）并把 `nlink` 减一；`fstat` 把 inode 编号、类型与链接数写回用户态 `Stat`。

需要说明：`unlinkat` 在 `nlink` 归零时没有回收数据块与 inode（测例 `ch6_file2` 注释明确允许不回收）。CI 测试 `make -C ci-user test CHAPTER=6` 全部通过（31/31），其中 `Test fstat OK`、`Test link OK`、`Test mass open/unlink OK` 分别对应本实验的三个系统调用。

## 具体实现

### 1. 磁盘 inode 增加链接计数，vfs inode 记录编号

| 改动 | 位置 | 说明 |
| --- | --- | --- |
| `nlink` 字段 | `easy-fs/src/layout.rs` 的 `DiskInode` | 在 `type_` 之后追加 `pub nlink: u32` |
| 初始化 | `DiskInode::initialize` | 置 1：新建文件/目录创建时恰好有一个目录项指向它 |
| inode 编号 | vfs `Inode` 新增 `inode_id` 字段 | `DirEntry` 存的是 inode 编号，`link`/`fstat` 都需要它；`Inode::new` 增加首参，`find`/`create` 传已知编号，`root_inode` 传 0 |

一个隐藏影响：`DiskInode` 从 128 字节变成 132 字节，`BLOCK_SZ = 512`，所以每个块容纳的 inode 由 4 个变成 3 个。好在 superblock 里的 `inode_area_blocks` 是用 `size_of::<DiskInode>()` 现算的，打包器（`easy-fs-fuse`）和内核共用同一份代码，只要重建 `fs.img` 就不会出现格式不一致。

同时给 vfs `Inode` 补了三个读接口，它们都通过 `read_disk_inode` 现读磁盘 inode，不做缓存：`inode_id()`、`nlink()`、`is_dir()`。另加私有 `find_dirent`，在目录的 `DiskInode` 上扫描目录项，返回 `(行号, inode_id)`，供 `unlink` 定位删除位置。

### 2. linkat

调用链：`sys_linkat` → `link_file` → `ROOT_INODE.link(new_name, target_inode_id)`。

`Inode::link` 的步骤：

| 顺序 | 操作 | 说明 |
| --- | --- | --- |
| 1 | `find_inode_id(new_name)` 查重 | 新名已存在返回 `false` |
| 2 | `increase_size` 扩容 + `write_at` 追加 `DirEntry::new(new_name, target_inode_id)` | 复用 `create` 的目录项追加路径，不分配新 inode、不初始化 |
| 3 | `get_disk_inode_pos(target_id)` → `get_block_cache` → `nlink += 1` | 目标 inode 的链接计数加一 |
| 4 | `block_cache_sync_all()` 落盘，返回 `true` | |

关键在于目录项里存的是 **inode 编号**，不是数据块指针，所以"两个名字指向同一个文件"只是多写一条 32 字节的 `DirEntry`，既不用复制数据，也不用重新分配 inode。`sys_linkat` 在 syscall 层先比较 `old_name == new_name` 返回 -1，再由 `link_file` 完成 `find(old_name)` 拿到目标 `inode_id`。

### 3. unlinkat（墓碑删除）

`Inode::unlink` 的步骤：

| 顺序 | 操作 | 说明 |
| --- | --- | --- |
| 1 | `find_dirent(name)` 找 `(index, target_id)` | 找不到返回 `false` |
| 2 | `dir.write_at(index * DIRENT_SZ, DirEntry::empty().as_bytes(), ...)` | **墓碑**：用全零目录项覆盖原槽位 |
| 3 | 目标 inode `nlink -= 1`，`block_cache_sync_all()` | |

**为什么用墓碑而不是搬移紧凑**：目录 inode 的 `size` 只能通过 `increase_size` 增大，`DiskInode` 没有 `reduce_size`，搬移后缩容需要手写"尾部数据块归还 + indirect 块回收"，复杂且易错。墓碑方案下 `find_dirent` 扫到全零项时 `name()` 返回 `""`，永远不会误匹配真实文件名，`find`/`open` 立即失效；代价是 `ls` 会多出一个空名字条目、目录空间不回收，测例不检查这两点。

**nlink 归零不回收**：`ch6_file2` 注释明确 "It's Ok if you don't delete the inode and data blocks"，因此只做计数减一。若要彻底回收，需要在归零时对目标 inode 调 `clear()`（已能回收全部数据块）并回收 inode 位图位，但 `clear()` 内部会再次获取 fs 锁，必须先释放外层 `fs` 锁再调用，否则自旋锁重入死锁。

### 4. fstat

`fstat` 要解决两个问题：从 `Arc<dyn File>` 拿到 inode 信息，以及把 `Stat` 写进用户地址空间。

- `fd_table` 存的是 `Arc<dyn File + Send + Sync>` trait object，`OSInode` 的方法无法直接调用。因此给 `File` trait 增加 `stat()` 方法并提供默认实现（返回 `mode = StatMode::NULL` 的空 `Stat`，供 `Stdin`/`Stdout` 使用）；`OSInode` 在 `impl File` 中覆写，组装 `dev = 0`、`ino = inode_id()`、`mode = FILE/DIR`、`nlink = nlink()`、`pad` 清零的 `Stat`。
- `sys_fstat` 按 `sys_write` 的模式校验 fd（越界或空槽返回 -1），`clone` 出 `Arc<dyn File>` 后 `drop(inner)` 释放 TCB 锁，再调 `file.stat()`。
- 写回用户态用 `translated_refmut(token, st)`：它把指针所在页经页表翻译成 `&'static mut Stat`，直接整体赋值。`Stat` 是 `#[repr(C)]` 共 80 字节，与用户态 `user_lib::Stat` 同构。

### 5. 系统调用分发表

`sys_linkat(old_name, new_name)` 和 `sys_unlinkat(name)` 忽略 `dirfd` 与 `flags`（本题固定为 `AT_FDCWD` 和 0），路径用 `translated_str` 从用户态逐字节拷到内核 `String`。`linkat`/`unlinkat`/`fstat` 均已在 `syscall/mod.rs` 分发表中接好（`SYSCALL_LINKAT = 37`、`SYSCALL_UNLINKAT = 35`、`SYSCALL_FSTAT = 80`），参数下标分别取 `args[1]/args[3]`、`args[1]`、`args[0]/args[1]`。

## 问答作业

### 1. easy-fs 中 root inode 的作用？如果 root inode 的内容损坏了会怎样？

**作用。** root inode 是整个文件系统的入口，编号固定为 0，由 `EasyFileSystem::create` 在格式化时第一个 `alloc_inode` 出来，类型初始化为目录。本实验的文件系统只有一层，root inode 就是“所有文件的父目录”：

- 内核在 `os/src/fs/inode.rs` 里用 `ROOT_INODE` 静态持有它，`open_file`、`list_apps`、`linkat`、`unlinkat` 全部从它出发；
- 它的数据块里依次排列着 `DirEntry`（名字 + inode 编号），`size / 32` 就是当前文件个数；
- 目录项 → inode 编号 → `get_disk_inode_pos(inode_id)`，这条链路的起点就是 root inode，所以它是“唯一不需要被别的目录项指向的 inode”。

**损坏的后果。** 因为除 superblock 的 `EFS_MAGIC` 之外没有任何校验，root inode 一旦损坏（`size` 被改成非法值、`direct` 里的块号指向别处、`type_` 被写成文件），表现是“文件系统还在，但谁也不认识”：

- `size` 异常偏大时，`find`/`ls` 会按 `size / DIRENT_SZ` 读出一堆垃圾 `DirEntry`，`DirEntry::name()` 里 `from_utf8(...).unwrap()` 直接 panic；
- `size` 偏小或指向错误块时，`open_file("initproc")` 找不到文件，`main` 里的 `unwrap()` panic，系统在启动阶段就死掉；
- 目录项里的 inode 编号若被破坏成越界值，`get_disk_inode_pos` 会算到 inode 区之外（甚至数据区），可能读到别的文件的内容，写操作则会把别的块覆盖掉，属于静默的数据损坏；
- 由于没有冗余副本（superblock 里也只有 inode 区/数据区的起始块号），root inode 损坏后无法自动修复，只能重新格式化镜像或者用 `easy-fs-fuse` 重新打包。

一句话：root inode 是“根目录的内容本身”，它坏了就等于没有可靠的目录树，文件系统整体不可用；而 superblock 坏了（magic 不匹配）至少会被 `assert!(super_block.is_valid())` 立刻发现并报错。

### 2.（ch7）举出使用 pipe 的一个实际应用

最典型的是 `cat file | wc -l`：shell 为这条命令建一个管道，`cat` 的 stdout 重定向到写端，`wc` 的 stdin 重定向到读端，`cat` 一边读文件一边写管道，`wc` 一边从管道读一边累加行数。文件不需要被完整读进内存，两个进程可以并行推进，pipe 自身几 KB 的缓冲把生产速度和消费速度解耦了。

同类例子很多：`ls | grep rs`（过滤）、`git log | less`（分页）、`dmesg | tail`、`ps aux | grep xxx`。它们的共同点是：

- 两个程序彼此不知道对方存在，只通过“标准的字节流”耦合，因此可以任意组合（Unix 的组合性）；
- 数据是流式的，不需要落盘的临时文件中转；
- 生产者和消费者速率不一致时，管道缓冲负责削峰：满了写端阻塞，空了读端阻塞。

### 3.（ch7）设计一个更易用的多进程通信机制

每对进程建一个管道的问题有三个：**地址发现**（事先不知道对方 pid/fd）、**拓扑**（N 个进程 → N² 条管道）、**消息边界**（pipe 是字节流，没有“一条消息”的概念）。可以设计一个“命名邮箱 + 订阅”的机制：

| 组件 | 设计 |
| --- | --- |
| 邮箱（mailbox） | 内核对象 `Mailbox { id, queue: VecDeque<Message>, refcnt, blocked_readers, blocked_writers }`，`Message` 带 `from`、`len`、`type`、可选优先级；队列有容量上限 |
| 命名 | `register(name) -> mailbox_id`、`lookup(name) -> mailbox_id`，避免硬编码 pid；名字表就是一层平铺的符号表 |
| 多对多 | 邮箱用引用计数管理：多个进程持同一个 id → 多个写者一个读者就是消息队列；一个写者多个读者则按 FIFO 分发，配一个 `semaphore` 控制可读消息数即可 |
| 订阅 | `subscribe(topic)` / `publish(topic, msg)`，内核（或用户态 broker）维护 topic → 订阅者列表，天然支持广播和一对多，发送方不需要知道接收方是谁 |
| 阻塞语义 | `send/recv` 带 `BLOCK` 标志；非阻塞时队列空/满返回错误码。与 pipe 不同，**保留消息边界**，接收方一次 `recv` 只拿到一条消息 |
| 大块数据 | 控制消息走邮箱，大数据走 `共享内存 + 邮箱里传一页编号`，避免多次拷贝；同步用邮箱本身就是“消息”，不需要额外锁 |
| 权限 | 只有持有邮箱 id 的进程能读写（类似 fd 表），防止任意进程打扰 |

对比现有方案：FIFO 需要文件系统配合；System V 消息队列靠 `key` 找队列，名字空间是全局的、难以回收；共享内存吞吐最高但同步全靠自己写；Unix socket 最灵活（还能跨机），但“先 bind 再 connect”对只想发一条消息的场景太重。我倾向“命名邮箱 + topic 订阅”作为默认抽象，底下用共享内存承载大消息：既解决了多对多的拓扑问题，也保住了消息边界和阻塞语义。

## 对本次实验的看法（选填）

- 硬链接把“文件内容”和“名字”彻底分离讲清楚了：`linkat` 改的是目录项和计数，数据块一个字节都不动；`unlinkat` 才是真正可能销毁数据的那一步。
- 本章最容易漏的是 `unlink` 的回收路径：直接块、indirect1/indirect2 及下挂块、inode 位图，少回收一类就会在 `ch6_file3` 反复建删大文件时耗尽空间，而单次简单测例完全测不出来。
- `DiskInode` 加字段会悄悄改变“每块能放几个 inode”，属于隐藏的磁盘格式变更；镜像必须重建这一点如果题面提一句，能省掉不少莫名其妙的 panic。
- 建议明确“新路径已存在”时 `linkat` 的返回值（现在写作未定义行为），以及 `unlinkat` 删除目录项后是否要求目录保持紧凑。
- 工作量适中：逻辑不难，难点集中在锁顺序（fs 锁与块缓存锁不能嵌套）和跨页写用户态的 `Stat` 上。

## 荣誉准则

1. 在完成本次实验的过程（含此前学习的过程）中，我曾分别与以下各位就（与本次实验相关的）以下方面做过交流，还在代码中对应的位置以注释形式记录了具体的交流对象及内容：

   《你交流的对象说明》

2. 此外，我也参考了以下资料，还在代码中对应的位置以注释形式记录了具体的参考来源及内容：

   《你参考的资料说明》

3. 我独立完成了本次实验除以上方面之外的所有工作，包括代码与文档。我清楚地知道，从以上方面获得的信息在一定程度上降低了实验难度，可能会影响其评分。

4. 我从未使用过他人的代码，不管是原封不动地复制，还是经过了某些等价转换。我未曾也不会向他人（含此后各届同学）复制或公开我的实验代码，我有义务妥善保管好它们。我提交至本实验的评测系统的代码，均无意于破坏或妨碍任何计算机系统的正常运转。我清楚地知道，以上情况均为本课程纪律所禁止，若违反，对应的实验成绩将按“-100”分计。
