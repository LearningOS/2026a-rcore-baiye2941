//! Process management syscalls
use crate::config::PAGE_SIZE;
use crate::mm::{
    frame_remaining, translated_byte_buffer, MapPermission, PageTable, PTEFlags, VirtAddr,
};
use crate::task::{
    change_program_brk, current_user_token, exit_current_and_run_next, get_syscall_count,
    mmap_memory, munmap_memory, suspend_current_and_run_next,
};
use crate::timer::get_time_us;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// task exits and submit an exit code
pub fn sys_exit(_exit_code: i32) -> ! {
    trace!("kernel: sys_exit");
    exit_current_and_run_next();
    panic!("Unreachable in sys_exit!");
}

/// current task gives up resources for other tasks
pub fn sys_yield() -> isize {
    trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel: sys_get_time");

    // 获取当前时间, 内核态读取
    let us = get_time_us();

    // 构造 TimeVal 结构体, 除以1_000_000 转换为秒, 取余数转换为微秒（内核态）
    let time = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };

    // 虚拟地址转换
    let ts_bytes = translated_byte_buffer(
        current_user_token(),
        ts as *const u8,
        core::mem::size_of::<TimeVal>(),
    );

    let src = &time as *const TimeVal as *const u8;
    let src = unsafe { core::slice::from_raw_parts(src, core::mem::size_of::<TimeVal>()) };

    let mut offset = 0;
    for slice in ts_bytes {
        let len = slice.len();
        slice.copy_from_slice(&src[offset..offset + len]);
        offset += len;
    }
    0
}

// 翻译用户虚拟地址
fn translate_user_addr(token: usize, va: VirtAddr, need_write: bool) -> Option<usize> {
    let page_table = PageTable::from_token(token);
    let pte = page_table.translate(va.floor())?;
    if !pte.is_valid() || !pte.flags().contains(PTEFlags::U) {
        return None;
    }
    if need_write {
        pte.writable()
            .then_some((pte.ppn().0 << 12) | va.page_offset())
    } else {
        pte.readable()
            .then_some((pte.ppn().0 << 12) | va.page_offset())
    }
}

/// TODO: Finish sys_trace to pass testcases
/// HINT: You might reimplement it with virtual memory management.
pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    trace!("kernel: sys_trace");

    // 将 id 包装为虚拟地址
    let va = VirtAddr::from(id);
    match trace_request {
        0 => match translate_user_addr(current_user_token(), va, false) {
            Some(addr) => unsafe { *(addr as *const u8) as isize },
            None => -1,
        },
        1 => match translate_user_addr(current_user_token(), va, true) {
            Some(addr) => unsafe {
                *(addr as *mut u8) = data as u8;
                0
            },
            None => -1,
        },
        // 2: 查询当前任务调用编号为 id 的系统调用次数
        2 => get_syscall_count(id) as isize,
        _ => -1,
    }
}

// YOUR JOB: Implement mmap.
pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    trace!("kernel: sys_mmap");
    // 错误1: start 必须按页对齐
    if !VirtAddr::from(start).aligned() {
        return -1;
    }
    // 错误2: prot 低 3 位之外必须是 0
    if prot & !0x7 != 0 {
        return -1;
    }
    // 错误3: 权限位全 0 的内存无意义
    if prot & 0x7 == 0 {
        return -1;
    }
    // len 为 0: 无事可做, 直接成功
    if len == 0 {
        return 0;
    }
    // 溢出保护: start + len 不得回绕
    if start.checked_add(len).is_none() {
        return -1;
    }
    // 错误5: 物理内存不足（预检, 题目要求不做失败回收）
    let need_pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    if frame_remaining() < need_pages {
        return -1;
    }
    // prot 编码 → MapPermission: 一位一位手动映射, 并总是带上 U
    let mut perm = MapPermission::U;
    if prot & 0b001 != 0 {
        perm |= MapPermission::R;
    }
    if prot & 0b010 != 0 {
        perm |= MapPermission::W;
    }
    if prot & 0b100 != 0 {
        perm |= MapPermission::X;
    }
    mmap_memory(start, len, perm)
}

// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel: sys_munmap");
    
    if !VirtAddr::from(start).aligned() {
        return -1;
    }
    if len == 0 {
        return 0;
    }
    if start.checked_add(len).is_none() {
        return -1;
    }
    // 区间含未映射页
    munmap_memory(start, len)
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel: sys_sbrk");
    if let Some(old_brk) = change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}
