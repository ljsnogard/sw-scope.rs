//! 集成测试：Scope 代际结构与清盘方案 A（关闭 + 静默后回收）。
//!
//! 与其它集成测试一样，用一把互斥锁串行化共享全局 root 的用例（弱池尚未做同步）。

use std::sync::Mutex;

use sw_scope::{Scope, TrScope};

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 验证 A 方案最关键的一条：**句柄还活着时，关闭不会释放数据**。
/// - 手段：父 Scope 造子 Scope，子在子域放入一个值；先析构子句柄、再析构父句柄。
/// - 判断：`Retain` 仍然可用且能读到原值——说明 `Drop` 只做了"标记关闭"，没有强制清盘。
#[test]
fn closing_scopes_keeps_live_retain_usable() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let parent = Scope::new_local();
    let mut child = parent.child_scope();
    let retain = child.put(42u64);

    drop(child);
    drop(parent);

    assert_eq!(
        *retain.try_owning().expect("A 方案下句柄仍应可用"),
        42,
        "关闭不能释放仍被引用的数据"
    );
}

/// 验证关闭深子树是**迭代**的：不会因为递归而爆栈。
/// - 手段：造一条 `DEPTH` 层深的子 Scope 链，析构最顶层的句柄（触发整条链的关闭）。
/// - 判断：整个测试跑完不 panic、不栈溢出。若关闭写成递归，`DEPTH = 100_000` 必然爆栈。
/// - 说明：其余句柄用 `forget` 泄漏掉——A 方案下它们仍然有效，逐个析构会带来 O(N²) 的
///   名单扫描，测试里没必要。
#[test]
fn deep_scope_chain_closes_iteratively() {
    const DEPTH: usize = 100_000;

    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let mut handles: Vec<Scope> = Vec::new();
    let mut cur = Scope::new_local();
    for _ in 0..DEPTH {
        let next = cur.child_scope();
        handles.push(cur);
        cur = next;
    }
    handles.push(cur);

    // handles[0] 是顶层，下面挂着 DEPTH 层
    let top = handles.remove(0);
    drop(top);

    // A 方案不会让其余句柄悬空；直接泄漏以免 O(N²) 的逐个 reclaim
    core::mem::forget(handles);
}

/// 默认（未开启 `strict-put-after-close`）：向已关闭的 Scope 放入会被**忽略**，照常成功。
#[cfg(not(feature = "strict-put-after-close"))]
#[test]
fn put_after_close_is_ignored_by_default() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let parent = Scope::new_local();
    let mut child = parent.child_scope();
    // 析构父句柄：子域被标记关闭，但子句柄仍活着
    drop(parent);

    let retain = child
        .try_put(|| 7u64)
        .expect("默认应当忽略关闭标记");
    assert_eq!(*retain.try_owning().expect("应能独占"), 7);
}

/// 开启 `strict-put-after-close`：向已关闭的 Scope 放入返回 [`sw_scope::ScopeError::ClosedScope`]。
#[cfg(feature = "strict-put-after-close")]
#[test]
fn put_after_close_errors_under_strict_feature() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let parent = Scope::new();
    let mut child = parent.child_scope();
    drop(parent);

    let result = child.try_put(|| 7u64);
    assert!(
        matches!(result, Err(sw_scope::ScopeError::ClosedScope)),
        "严格模式下应当返回 ClosedScope"
    );
}
