use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::sync::{MutexProbe, SemProbe};
use crate::task::{current_task, ProcessControlBlock};

/// 收集进程中所有存活线程的 tid（tasks 下标即 tid）
fn alive_tids(process: &Arc<ProcessControlBlock>) -> Vec<usize> {
    let process_inner = process.inner_exclusive_access();
    process_inner
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| {
            // 只保留拥有资源的活跃线程（已退出线程 res 为 None）
            task.as_ref()
                .map_or(false, |t| t.inner_exclusive_access().res.is_some())
        })
        .map(|(id, _)| id)
        .collect()
}

/// tid -> 矩阵行号
fn tid_index(tids: &[usize], tid: usize) -> Option<usize> {
    tids.iter().position(|t| *t == tid)
}

/// 银行家算法：返回 true 表示安全（存在让所有线程完成的调度序列）
fn is_safe(available: &[isize], allocation: &[Vec<isize>], need: &[Vec<isize>]) -> bool {
    let n = allocation.len();
    let m = available.len();
    let mut work = available.to_vec();
    let mut finish = vec![false; n];
    loop {
        let mut progressed = false;
        for i in 0..n {
            if finish[i] {
                continue;
            }
            if (0..m).all(|j| need[i][j] <= work[j]) {
                for j in 0..m {
                    work[j] += allocation[i][j];
                }
                finish[i] = true;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    finish.iter().all(|f| *f)
}

fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

/// 检测当前进程的 mutex 资源是否会死锁
/// pending_mutex: 当前线程正在请求但尚未进入等待队列的 mutex_id
/// 返回 true 表示不安全（死锁）
pub fn check_mutex_deadlock(
    process: &Arc<ProcessControlBlock>,
    pending_mutex: Option<usize>,
) -> bool {
    let tids = alive_tids(process);
    let n = tids.len();
    let probes: Vec<Option<MutexProbe>> = {
        let process_inner = process.inner_exclusive_access();
        process_inner
            .mutex_list
            .iter()
            .map(|m| m.as_ref().and_then(|m| m.probe()))
            .collect()
    };
    let m = probes.len();

    let mut available = vec![0isize; m];
    let mut allocation = vec![vec![0isize; m]; n];
    let mut need = vec![vec![0isize; m]; n];

    // 当前线程正在请求的资源也要计入 Need
    if let Some(j) = pending_mutex {
        if let Some(i) = tid_index(&tids, current_tid()) {
            need[i][j] = 1;
        }
    }

    for (j, probe) in probes.iter().enumerate() {
        let Some(pr) = probe else {
            continue; // 空槽位 / MutexSpin 不参与检测
        };
        available[j] = if pr.locked { 0 } else { 1 };
        if let Some(owner) = pr.owner {
            if let Some(i) = tid_index(&tids, owner) {
                allocation[i][j] = 1;
            }
            // owner 是已死线程 -> 找不到行 -> 资源无人归还 -> 自然判不安全
        }
        for w in &pr.waiters {
            if let Some(i) = tid_index(&tids, *w) {
                need[i][j] = 1;
            }
        }
    }

    !is_safe(&available, &allocation, &need)
}

/// 检测当前进程的 semaphore 资源是否会死锁
/// pending_sem: 当前线程正在请求但尚未进入等待队列的 semaphore_id
/// 返回 true 表示不安全（死锁）
pub fn check_semaphore_deadlock(
    process: &Arc<ProcessControlBlock>,
    pending_sem: Option<usize>,
) -> bool {
    let tids = alive_tids(process);
    let n = tids.len();
    let probes: Vec<Option<SemProbe>> = {
        let process_inner = process.inner_exclusive_access();
        process_inner
            .semaphore_list
            .iter()
            .map(|s| s.as_ref().map(|s| s.probe()))
            .collect()
    };
    let m = probes.len();

    let mut available = vec![0isize; m];
    let mut allocation = vec![vec![0isize; m]; n];
    let mut need = vec![vec![0isize; m]; n];

    // 当前线程正在请求的资源也要计入 Need
    if let Some(j) = pending_sem {
        if let Some(i) = tid_index(&tids, current_tid()) {
            need[i][j] = 1;
        }
    }

    for (j, probe) in probes.iter().enumerate() {
        let Some(pr) = probe else {
            continue; // 空槽位跳过
        };
        // count 可能为负（等待者已预支），可利用资源不能小于 0
        available[j] = pr.count.max(0);
        for (tid, units) in &pr.holders {
            if let Some(i) = tid_index(&tids, *tid) {
                allocation[i][j] = *units as isize;
            }
        }
        for w in &pr.waiters {
            if let Some(i) = tid_index(&tids, *w) {
                need[i][j] = 1;
            }
        }
    }

    let safe = is_safe(&available, &allocation, &need);
    println!(
        "[deadlock-sem] tids={:?} avail={:?} alloc={:?} need={:?} safe={}",
        tids, available, allocation, need, safe
    );
    !safe
}
