//! 域的表示（[`ScopeInner`]）与整棵树的根（[`RootScope`]）。
//!
//! # 新设计（重构中）
//!
//! 旧的"代际树"结构（父域 / 子域 / 兄弟链）正在被移除：现在一个 [`RootScope`] 通过一棵
//! [`bplustree_cmp::BPlusTree`] 管理整棵树里的所有 [`ScopeInner`]，树的键就是域地址。因此
//!
//! - [`Scope`](crate::Scope) 只保存指向自己 `ScopeInner` 的指针；
//! - [`ScopeInner`] 不再带 `const CELL_SIZE` / 配置类型等泛型参数；
//! - 分配器统一擦除为 `&'static dyn Allocator`，不再靠固定偏移反推 root。
//!
//! 旧实现先以注释保留在各子模块中，待新树的操作补齐后再逐段删除。当前编译期骨架里，
//! 尚未迁移完的运行路径以 `todo!()` 标记。
//!
//! # 子模块
//!
//! - 本文件：常量、[`PoolIndex`]、[`ScopeInner`] 字段定义与逐项导出；
//! - [`alloc_`](alloc_)：每域弱池取槽、每域强池 bump 分配与对象构造管线；
//! - [`root_`](root_)：新 [`RootScope`] 与由 B+ 树管理域的入口；
//! - `life_` / `teardown_`：旧代际树的生命周期与清盘逻辑，依赖已移除的父子链字段，
//!   暂时停用但保留源码。

use core::{ptr, sync::atomic::AtomicPtr};

// 旧的 `config_` / `life_` / `teardown_` 都建立在父子 / 兄弟链之上；新设计改由 root 的
// B+ 树管理所有域，这些模块暂时不参与编译。原文件保留，等树操作补齐后再决定迁移方式。
// `alloc_` 已按新设计（每域自持弱池 / 强池、新强块布局）重写并重新启用。
mod alloc_;
mod config_;
mod inner_;
// mod life_;
// mod teardown_;
mod root_;

// 旧的单元测试直接构造 `ScopeInner<8>` / `RootExtension`，与新设计不兼容；先停用，
// 待新的树级测试补齐后再恢复。
//
// #[cfg(test)]
// mod tests_;

pub use config_::{DefaultConfig, TrScopeInnerConfig};
pub(crate) use inner_::{ScopeHeader, ScopeInner};
pub(crate) use root_::RootScope;

// 旧导出（仅供对照）：
// pub(crate) use root_::{RootScope, RootScopeRef};
// pub(crate) use root_::RootExtension;

pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub const DEFAULT_CELL_SIZE: usize = core::mem::size_of::<usize>();

pub(crate) type PoolIndex = u16;

/// 默认 root 的发布点（指向 [`RootScope`]）；空指针表示尚未初始化。
pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<RootScope> = AtomicPtr::new(ptr::null_mut());


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
