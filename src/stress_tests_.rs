//! 压力单元测试：多个 1KB 页的 Scope 各自持有长强池 / 弱池链，跨 Scope 引用后并发 close。
//!
//! 目标不是证明“某个固定耗时”，而是：
//! - 在大量池链与跨 Scope `Retain` 下，`Scope::close` 不会死锁；
//! - 记录一次 close 的墙钟耗时，为是否引入 `close_async` / 自旋锁提供数据；
//! - 外部 `Retain` 全部释放后，所有对象恰好析构一次。

use std::{
    alloc::Global,
    eprintln,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{Retain, Scope, TrScopeInnerConfig, page_size::PageSize1KB};

/// 1KB 页的测试配置；每域仍使用 `Global` 作为池分配器。
struct TinyPageConfig;

impl TrScopeInnerConfig for TinyPageConfig {
    type PoolAlloc = Global;
    type LockAlloc = Global;
    type PageSize = PageSize1KB;
}

/// 压力用例的全局析构计数。
static STRESS_DROPS: AtomicUsize = AtomicUsize::new(0);

/// 跨 Scope 链表节点。
struct StressNode {
    value: usize,
    prev: Option<Retain<StressNode>>,
    next: Option<Retain<StressNode>>,
}

impl StressNode {
    fn new(value: usize) -> Self {
        StressNode {
            value,
            prev: None,
            next: None,
        }
    }
}

impl Drop for StressNode {
    fn drop(&mut self) {
        let _ = self.value;
        STRESS_DROPS.fetch_add(1, Ordering::AcqRel);
    }
}

/// 测试多个小页 Scope 的长池链与跨域引用，在并发 close 时不发生死锁。
/// Test concurrent close of many small-page Scopes with long strong/weak pool chains and
/// cross-scope references, checking for deadlock.
///
/// - 手段：创建 4 个 1KB 页的 Scope，每个放入 3000 个节点（共 12000 个），按“每个下标
///   跨 4 个 Scope 成环”的方式建立跨域引用。保留全部外部 `Retain`，用屏障让 4 个线程
///   同时调用 `Scope::close`，主线程用 `recv_timeout` 检测是否卡死，并记录总耗时与单线程
///   最大耗时。close 完成后释放全部外部 `Retain`。
///   / How: create four 1KB-page Scopes with 3000 nodes each (12000 total), link each index
///   into a cross-scope four-node cycle, keep all external `Retain`s, and let four threads call
///   `Scope::close` simultaneously behind a barrier. The main thread uses `recv_timeout` to
///   detect a deadlock and records total and max close latency, then drops every external
///   `Retain`.
/// - 判断：4 个 close 都在超时前返回；close 总耗时低于保守上界 30s；close 期间无节点析构；
///   完成后 12000 个节点恰好析构一次，释放失效外部 `Retain` 不再增加计数。
///   / What: all four closes return before the timeout, total close latency is below a
///   conservative 30s bound, no node drops during close, and exactly 12000 nodes drop after
///   close; releasing the now-dead external Retains does not add drops.
#[test]
fn many_scopes_small_pages_close_concurrently_without_deadlock() {
    const SCOPES: usize = 4;
    const PER_SCOPE: usize = 3000;
    const TOTAL: usize = SCOPES * PER_SCOPE;

    STRESS_DROPS.store(0, Ordering::Release);

    let create_start = Instant::now();
    let scopes: std::vec::Vec<Scope> = (0..SCOPES)
        .map(|_| Scope::new_with_cfg::<TinyPageConfig>())
        .collect();
    let mut nodes: std::vec::Vec<std::vec::Vec<Retain<StressNode>>> =
        std::vec::Vec::with_capacity(SCOPES);
    for scope in scopes.iter() {
        let mut per_scope = std::vec::Vec::with_capacity(PER_SCOPE);
        for index in 0..PER_SCOPE {
            per_scope.push(
                scope
                    .put(StressNode::new(index))
                    .expect("压力节点放入应当成功"),
            );
        }
        nodes.push(per_scope);
    }
    // 每个下标 i 的节点在 4 个 Scope 之间成环：
    // nodes[s][i].prev = nodes[s-1][i]，nodes[s][i].next = nodes[s+1][i]。
    for scope_index in 0..SCOPES {
        for index in 0..PER_SCOPE {
            let prev = nodes[(scope_index + SCOPES - 1) % SCOPES][index].clone();
            let next = nodes[(scope_index + 1) % SCOPES][index].clone();
            let mut owning = nodes[scope_index][index]
                .try_owning()
                .expect("压力节点应当可独占");
            owning.prev = Some(prev);
            owning.next = Some(next);
        }
    }
    let create_elapsed = create_start.elapsed();
    assert_eq!(STRESS_DROPS.load(Ordering::Acquire), 0, "建链阶段不应析构");

    // 保留 nodes 作为外部 Retain：close 期间它们保证每个槽位的弱计数非零，从而不会让
    // 某个 Scope 的关闭线程在另一个 Scope 的锁外级联析构数据。
    let barrier = Arc::new(Barrier::new(SCOPES + 1));
    let (done_tx, done_rx) = mpsc::channel::<Duration>();
    let mut threads = std::vec::Vec::with_capacity(SCOPES);
    for scope in scopes {
        let barrier = Arc::clone(&barrier);
        let done_tx = done_tx.clone();
        threads.push(thread::spawn(move || {
            barrier.wait();
            let started = Instant::now();
            scope.close();
            let _ = done_tx.send(started.elapsed());
        }));
    }
    drop(done_tx);

    let close_start = Instant::now();
    barrier.wait();
    let mut per_thread = std::vec::Vec::with_capacity(SCOPES);
    for _ in 0..SCOPES {
        per_thread.push(
            done_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("Scope::close 超时，疑似死锁或严重锁竞争"),
        );
    }
    for thread in threads {
        thread.join().expect("close 线程不应 panic");
    }
    let close_total = close_start.elapsed();
    let close_max = per_thread.iter().copied().max().unwrap_or_default();

    eprintln!(
        "[close_stress] scopes={SCOPES} per_scope={PER_SCOPE} total={TOTAL} \
         create={create_elapsed:?} close_total={close_total:?} close_max={close_max:?}"
    );

    assert!(
        close_total < Duration::from_secs(30),
        "并发 close 总耗时 {close_total:?} 超过 30s 保守上界"
    );
    assert_eq!(
        STRESS_DROPS.load(Ordering::Acquire),
        TOTAL,
        "close 完成后所有节点必须恰好析构一次"
    );

    // 外部 Retain 已经指向 Destroyed 槽位；释放它们不应再次析构或再次扫描整条池链。
    let release_start = Instant::now();
    drop(nodes);
    let release_elapsed = release_start.elapsed();
    assert_eq!(
        STRESS_DROPS.load(Ordering::Acquire),
        TOTAL,
        "释放失效 Retain 不得重复析构"
    );
    eprintln!("[close_stress] release_dead_retains={release_elapsed:?} drops={TOTAL}");
}
