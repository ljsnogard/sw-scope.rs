use core::{
    alloc::Allocator,
    ptr::NonNull,
    sync::atomic::AtomicUsize,
};

use atomex::LocksOrderings;
use atomic_sync::rwlock::cooperative_arena::CooperativeRwLock;

use crate::{
    scope_inner_::config_::TrScopeInnerConfig,
    strong_::StrongPool,
    weak_::WeakPool,
    x_deps::{atomex, atomic_sync},
};

/// 一个域的内部表示。它总是与一个 Allocator 一起分配，组成 (ScopeInner, Allocator)。
/// 因此，任何时候都必须保证 ScopeInner 的
///
/// 新设计里它由 root 的 [`bplustree_cmp::BPlusTree`] 统一持有并索引，因此不再保存
/// 父域 / 子域 / 兄弟链；这些字段的旧版本见文件末尾注释以及 `life_` / `teardown_` 的源码。
///
/// 域持有若干 [`StrongPool`] 组成的链，池内的对象位置终身不变（见 `StrongPool` 的文档）；
/// 域静默时整条链一次性回收。
///
/// 数据对象的"存活链"挂在弱槽位上（`WeakSlot::prev_live_` / `next_live_`），这里只保存
/// 链头与链尾。
#[repr(C)]
pub(crate) struct ScopeInner {
    /// 域标志位：`CLOSED` / `HANDLE_DROPPED`。
    flags_: AtomicUsize,
    /// 指向弱槽位池链尾。
    weak_head_: Option<NonNull<WeakPool>>,
    weak_tail_: Option<NonNull<WeakPool>>,

    /// 指向 Scope 所持有的第一块强池。
    strong_head_: Option<NonNull<StrongPool>>,
    strong_tail_: Option<NonNull<StrongPool>>,

    /// 用于分配 WeakPool 和 StrongPool 的内存分配器
    allocator_: &'static dyn Allocator,
}

type ScopeRwLock<C> = CooperativeRwLock<
    (),
    usize,
    core::sync::atomic::AtomicUsize,
    LocksOrderings,
    <C as TrScopeInnerConfig>::LockAlloc,
>;

#[repr(C)]
pub(crate) struct ScopeInnerWithCfg<C>
where
    C: TrScopeInnerConfig,
{
    inner_: ScopeInner,
    rwlock_: ScopeRwLock<C>,
    alloc_: C::PoolAlloc,
}
