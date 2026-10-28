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
//! - [`teardown_`](teardown_)：`Scope::close` 之后的延迟清盘；只处理本域弱池链上的
//!   槽位，不依赖父子域。
//!
//! `life_` 仍是旧代际树的实现，暂时保留源码但不参与编译。

use core::{
    ptr::{self, NonNull},
    sync::atomic::AtomicPtr,
};

use crate::weak_::{WeakPool, WeakSlot};

// `life_` 仍建立在旧的父子 / 兄弟链之上，暂时不参与编译；`teardown_` 已按新设计重写，
// 只依赖每域自持的弱池 / 强池链与新的类型擦除析构入口。
mod alloc_;
mod config_;
mod inner_;
// mod life_;
mod root_;
mod teardown_;

// 旧的单元测试直接构造 `ScopeInner<8>` / `RootExtension`，与新设计不兼容；先停用，
// 待新的树级测试补齐后再恢复。
//
// #[cfg(test)]
// mod tests_;

pub use config_::{DefaultConfig, TrScopeInnerConfig};
pub(crate) use inner_::{ScopeEntity, ScopeInner, ScopeLock};
pub(crate) use root_::RootScope;

// 旧导出（仅供对照）：
// pub(crate) use root_::{RootScope, RootScopeRef};
// pub(crate) use root_::RootExtension;

pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub const DEFAULT_CELL_SIZE: usize = core::mem::size_of::<usize>();

pub(crate) type PoolIndex = u16;

/// 默认 root 的发布点（指向 [`RootScope`]）；空指针表示尚未初始化。
pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<RootScope> = AtomicPtr::new(ptr::null_mut());

/// 由弱槽位找到所属域，并在该域已请求关闭时重试延迟清盘。
///
/// 只用于 `Owning` / `Sharing` 释放后的重试；此时清盘循环会跳过仍然存活的直接视图。
pub(crate) fn try_flush_for_slot_<T: ?Sized>(weak: &WeakSlot<T>) {
    let slot = NonNull::from(weak).cast::<WeakSlot<()>>();
    let Option::Some(pool) = WeakPool::of_slot_(slot) else {
        return;
    };
    // SAFETY: 槽位来自该池，池仍由所属域持有
    let Option::Some(scope) = unsafe { pool.as_ref() }.root() else {
        return;
    };
    // SAFETY: scope 指向存活实体内部的 ScopeInner
    unsafe { scope.as_ref() }.try_flush_();
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
