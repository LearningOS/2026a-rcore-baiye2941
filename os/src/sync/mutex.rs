//! Mutex (spin-like and blocking(sleep))

use super::UPSafeCell;
use crate::task::TaskControlBlock;
use crate::task::{block_current_and_run_next, suspend_current_and_run_next};
use crate::task::{current_task, wakeup_task};
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex
    fn lock(&self);
    /// Unlock the mutex
    fn unlock(&self);
    /// 返回一个MutexProbe，用于检测死锁
    fn probe(&self) -> Option<MutexProbe>;
}


/// 获取 Mutex 的快照，用于检测死锁
pub struct MutexProbe {
    /// 是否被占用
    pub locked: bool,
    /// 持有者TID
    pub owner: Option<usize>,
    /// 等待队列中的TID
    pub waiters: Vec<usize>,
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: UPSafeCell<bool>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        Self {
            locked: unsafe { UPSafeCell::new(false) },
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) {
        trace!("kernel: MutexSpin::lock");
        loop {
            let mut locked = self.locked.exclusive_access();
            if *locked {
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                return;
            }
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.exclusive_access();
        *locked = false;
    }

    fn probe(&self) -> Option<MutexProbe> {
        None  // 自旋锁不参与死锁检测
    }
}

/// Blocking Mutex struct
pub struct MutexBlocking {
    inner: UPSafeCell<MutexBlockingInner>,
}

pub struct MutexBlockingInner {
    locked: bool,
    owner: Option<usize>, // 持有者TID
    wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl MutexBlocking {
    /// Create a new blocking mutex
    pub fn new() -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            inner: unsafe {
                UPSafeCell::new(MutexBlockingInner {
                    locked: false,
                    owner: None,
                    wait_queue: VecDeque::new(),
                })
            },
        }
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    fn lock(&self) {
        trace!("kernel: MutexBlocking::lock");
        let mut mutex_inner = self.inner.exclusive_access();
        if mutex_inner.locked {
            mutex_inner.wait_queue.push_back(current_task().unwrap());
            drop(mutex_inner);  // 放锁，防止死锁
            block_current_and_run_next();
        } else {
            mutex_inner.locked = true;
            mutex_inner.owner = Some(current_tid());
        }
    }

    /// unlock the blocking mutex
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.exclusive_access();
        assert!(mutex_inner.locked);
        if let Some(waking_task) = mutex_inner.wait_queue.pop_front() {
            mutex_inner.owner = Some(tid_of(&waking_task));
            wakeup_task(waking_task);
        } else {
            // 没有线程在等待，释放锁
            mutex_inner.locked = false;
            mutex_inner.owner = None;
        }
    }

    fn probe(&self) -> Option<MutexProbe> {
        let mutex_inner = self.inner.exclusive_access();
        Some(MutexProbe {
            locked: mutex_inner.locked,
            owner: mutex_inner.owner,   
            waiters: mutex_inner.wait_queue.iter().map(|task| tid_of(task)).collect(),
        })
    }
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
