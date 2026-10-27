//! 域的表示（[`ScopeInner`]）与整棵树的根（[`RootScope`]）。
//!
//! # 新设计（重构中）
//!
//! 旧的"代际树"结构（父域 / 子域 / 兄弟链）正在被移除：现在一个 [`RootScope`] 通过一棵
//! [`bplustree_cmp::BPlusTree`] 管理整棵树里的所有 [`ScopeInner`]，键是 [`ScopeKey`]。因此
//!
//! - [`Scope`](crate::Scope) 只保留"所属 root + 在树中的键"；
//! - [`ScopeInner`] 不再带 `const CELL_SIZE` / 配置类型等泛型参数；
//! - 分配器统一擦除为 `&'static dyn Allocator`，不再靠固定偏移反推 root。
//!
//! 旧实现先以注释保留在各子模块中，待新树的操作补齐后再逐段删除。当前编译期骨架里，
//! 尚未迁移完的运行路径以 `todo!()` 标记。
//!
//! # 子模块
//!
//! - 本文件：常量、[`PoolIndex`] / [`ScopeKey`]、[`ScopeInner`] 字段定义与逐项导出；
//! - [`alloc_`](alloc_)：强池 bump 分配、树共享弱池取槽与对象构造管线（旧实现，待迁移）；
//! - [`root_`](root_)：新 [`RootScope`] 与由 B+ 树管理域的入口；
//! - `life_` / `teardown_`：旧代际树的生命周期与清盘逻辑，依赖已移除的父子链字段，
//!   暂时停用但保留源码。

use core::{
    alloc::Allocator,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize},
};

use crate::{
    strong_::StrongPool,
    weak_::{WeakPool, WeakSlot},
};

// 旧的 `config_` / `alloc_` / `life_` / `teardown_` 都建立在父子 / 兄弟链与 const-generic
// 池之上；新设计改由 root 的 B+ 树管理所有域，因此这些模块暂时不参与编译。原文件保留，
// 等树操作补齐后再决定迁移方式。
//
// mod config_;
// mod alloc_;
// mod life_;
// mod teardown_;
mod root_;

// 旧的单元测试直接构造 `ScopeInner<8>` / `RootExtension`，与新设计不兼容；先停用，
// 待新的树级测试补齐后再恢复。
//
// #[cfg(test)]
// mod tests_;
pub use root_::RootScope;

// 旧导出（仅供对照）：
// pub(crate) use root_::{RootScope, RootScopeRef};
// pub(crate) use root_::RootExtension;

pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub const DEFAULT_CELL_SIZE: usize = core::mem::size_of::<usize>();

pub(crate) type PoolIndex = u16;

/// 域在 root 的 B+ 树中的键。
///
/// 旧设计用父子 / 兄弟链表达域之间的关系；新设计用它作为
/// [`bplustree_cmp::BPlusTree`] 的键，root 通过
/// `ScopeKey -> NonNull<ScopeInner>` 管理所有域。
// TODO(新设计): 确认键的生成策略（单调递增 / 复用回收域留下的空洞）。
pub(crate) type ScopeKey = usize;

/// 默认 root 的发布点（指向 [`RootScope`]）；空指针表示尚未初始化。
pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<RootScope> = AtomicPtr::new(ptr::null_mut());

/// 一个域的内部表示。
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
pub struct ScopeInner {
    /// 域标志位：`CLOSED` / `HANDLE_DROPPED`。
    flags_: AtomicUsize,

    /// 本域在 root 的 B+ 树中的键；创建时由 root 分配。
    scope_key_: ScopeKey,

    /// 指向本树共享的弱槽位池链头（旧字段，迁移期间保留）。
    weak_pool_: Option<NonNull<WeakPool>>,

    /// 指向 Scope 所持有的第一块强池。
    chain_head_: Option<NonNull<StrongPool>>,

    /// 指向 Scope 所持有的最新一块强池。
    chain_tail_: Option<NonNull<StrongPool>>,

    /// 存活链链头（弱槽位）。
    live_head_: Option<NonNull<WeakSlot<()>>>,

    /// 存活链链尾（弱槽位）。
    live_tail_: Option<NonNull<WeakSlot<()>>>,

    /// 指向 root 的具体分配器（已擦除）；既用于分配，也用于回到 root 的 B+ 树。
    allocator_: &'static dyn Allocator,
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 旧设计的字段（父域 / 兄弟链 / 待回收名单）——暂时保留在这里作对照，不再参与布局。
//
// #[repr(C)]
// pub struct ScopeInner<C>
// where
//     C: Sized + TrScopeInnerConfig,
// {
//     flags_: AtomicUsize,
//     weak_pool_: Option<NonNull<WeakPool>>,
//     strong_head_: Option<NonNull<StrongPool>>,
//     allocator_: C::PoolAlloc,
// }
//
// 更早的 const-generic 版本：
//
// pub struct ScopeInner<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> {
//     flags_: AtomicUsize,
//     parent_scope_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     prev_sibling_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     next_sibling_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     first_child_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     last_child_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     pending_head_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     pending_tail_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     pending_next_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
//     chain_head_: Option<NonNull<StrongPool<CELL_SIZE>>>,
//     chain_tail_: Option<NonNull<StrongPool<CELL_SIZE>>>,
//     live_head_: Option<NonNull<WeakChunk<()>>>,
//     live_tail_: Option<NonNull<WeakChunk<()>>>,
//     allocator_: &'static dyn Allocator,
// }
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
