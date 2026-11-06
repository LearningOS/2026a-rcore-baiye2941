//! Semaphore

use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

/// semaphore structure
pub struct Semaphore {
    /// semaphore inner
    pub inner: UPSafeCell<SemaphoreInner>,
}

pub struct SemaphoreInner {
    pub count: isize,
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
    /// 每个线程持有的单位数：(tid, count)，用于死锁检测建模
    pub holders: Vec<(usize, usize)>,
}

/// 信号量状态快照，供死锁检测使用
pub struct SemProbe {
    /// 当前可用单位数（可为负，表示有线程在排队）
    pub count: isize,
    /// 等待队列中的线程 tid 列表
    pub waiters: Vec<usize>,
    /// 各线程持有的单位数：(tid, count)
    pub holders: Vec<(usize, usize)>,
}

/// get current thread id
fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

/// get tid of an arbitrary task
fn tid_of(task: &Arc<TaskControlBlock>) -> usize {
    task.inner_exclusive_access().res.as_ref().unwrap().tid
}

/// 记录线程 tid 获取了一个单位
fn record_hold(inner: &mut SemaphoreInner, tid: usize) {
    if let Some(entry) = inner.holders.iter_mut().find(|(t, _)| *t == tid) {
        entry.1 += 1;
    } else {
        inner.holders.push((tid, 1));
    }
}

/// 归还一个单位：优先算调用者自己持有的；
/// 信号量单位匿名，调用者自己没有时释放任意一个持有者的单位
fn release_hold(inner: &mut SemaphoreInner, tid: usize) {
    let pos = inner
        .holders
        .iter()
        .position(|(t, c)| *t == tid && *c > 0)
        .or_else(|| inner.holders.iter().position(|(_, c)| *c > 0));
    if let Some(pos) = pos {
        inner.holders[pos].1 -= 1;
    }
    inner.holders.retain(|(_, c)| *c > 0);
}

impl Semaphore {
    /// Create a new semaphore
    pub fn new(res_count: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(SemaphoreInner {
                    count: res_count as isize,
                    wait_queue: VecDeque::new(),
                    holders: Vec::new(),
                })
            },
        }
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let mut inner = self.inner.exclusive_access();
        inner.count += 1;
        // 归还一个单位
        release_hold(&mut inner, current_tid());
        if inner.count <= 0 {
            if let Some(task) = inner.wait_queue.pop_front() {
                // 被唤醒的线程完成了一次 down，视为获取了一个单位
                let tid = tid_of(&task);
                record_hold(&mut inner, tid);
                wakeup_task(task);
            }
        }
    }

    /// down operation of semaphore
    pub fn down(&self) {
        trace!("kernel: Semaphore::down");
        let mut inner = self.inner.exclusive_access();
        inner.count -= 1;
        if inner.count < 0 {
            inner.wait_queue.push_back(current_task().unwrap());
            drop(inner);
            block_current_and_run_next();
        } else {
            // 立即获取成功，记录持有者
            record_hold(&mut inner, current_tid());
        }
    }

    /// 返回信号量状态快照供死锁检测使用
    pub fn probe(&self) -> SemProbe {
        let inner = self.inner.exclusive_access();
        SemProbe {
            count: inner.count,
            waiters: inner.wait_queue.iter().map(|t| tid_of(t)).collect(),
            holders: inner.holders.clone(),
        }
    }
}
