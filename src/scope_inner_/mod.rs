//! 域的表示（[`ScopeInner`]）与整棵树的根（[`RootScope`]）。
//!
//! # 多 root 设计
//!
//! 旧的"代际树"结构（父域 / 子域 / 兄弟链）已经移除：现在一个 [`RootScope`] 通过自己的
//! 实体池管理整棵树里的所有 [`ScopeInner`]，类型表也是 per-root 的。因此
//!
//! - [`Scope`](crate::Scope) 只保存指向自己 `ScopeInner` 的指针；
//! - [`ScopeInner`] 不再带 `const CELL_SIZE` / 配置类型等泛型参数；
//! - 分配器统一擦除为 `&'static dyn Allocator`，不再靠固定偏移反推 root；
//! - 弱槽位沿 `WeakSlot -> WeakPool.scope_ -> ScopeInner.root_ -> RootScope.metas_`
//!   找回自己的 root，不再使用任何进程级默认 root。
//!
//! # 子模块
//!
//! - 本文件：常量、[`PoolIndex`] 与逐项导出；
//! - [`alloc_`](alloc_)：每域弱池取槽、每域强池 bump 分配与对象构造管线；
//! - [`root_`](root_)：多实例 [`RootScope`]、发布槽位、per-root 类型表与最终清理；
//! - [`teardown_`](teardown_)：`Scope::close` 之后的延迟清盘；只处理本域弱池链上的
//!   槽位，不依赖父子域。
//!
//! `life_` 仍是旧代际树的实现，暂时保留源码但不参与编译。

use core::ptr::NonNull;

use crate::weak_::{WeakPool, WeakSlot};

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
pub(crate) use root_::RootLease;
pub use root_::RootScope;

pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub const DEFAULT_CELL_SIZE: usize = core::mem::size_of::<usize>();

pub(crate) type PoolIndex = u16;

/// 由弱槽位找到所属域，并在该域已请求关闭时重试延迟清盘。
///
/// 只用于 `Owning` / `Sharing` 释放后的重试；此时清盘循环会跳过仍然存活的直接视图。
/// 若本次重试让整棵树的最后一个域退出，则由当前线程在完全退出实体之后完成 root 清理。
pub(crate) fn try_flush_for_slot_<T: ?Sized>(weak: &WeakSlot<T>) {
    let slot = NonNull::from(weak).cast::<WeakSlot<()>>();
    let Option::Some(pool) = WeakPool::of_slot_(slot) else {
        return;
    };
    // SAFETY: 槽位来自该池，池仍由所属域持有
    let Option::Some(scope) = unsafe { pool.as_ref() }.scope() else {
        return;
    };
    // SAFETY: scope 指向存活实体内部的 ScopeInner；Retain 仍持有槽位，因此实体尚未退出
    if let Option::Some(root) = unsafe { scope.as_ref() }.try_flush_() {
        // SAFETY: 本线程是最后一个完成清空的域；此时不得再访问该实体
        unsafe { RootScope::finalize_(root) };
    }
}
