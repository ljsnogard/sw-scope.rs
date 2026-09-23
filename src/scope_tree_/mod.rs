//! 域的表示（[`ScopeInner`]）与整棵树的根（[`RootExtension`] / [`RootScope`]）。
//!
//! 拆分依据是"紧密共享同一组字段的操作放在一起"：
//!
//! - 本文件：常量、[`PoolIndex`]、[`ScopeInner`] 的字段定义与逐项导出；
//! - [`life_`](life_)：域标志与父子 / 兄弟链（代际结构）；
//! - [`alloc_`](alloc_)：强池 bump 分配、树共享弱池取槽与对象构造管线；
//! - [`teardown_`](teardown_)：清盘方案 A 的关闭、静默判定与回收；
//! - [`root_`](root_)：root 的共享前缀、分配器门面与一次性初始化，以及由域定位 root。
//!
//! 子模块是 `mod.rs` 的后代，因此可以直接访问 [`ScopeInner`] 的私有字段；模块外一律通过
//! 关联函数访问。

use core::{ptr, sync::atomic::AtomicPtr};

mod scope_node_;
mod root_node_;
mod teardown_;
mod tree_node_;

#[cfg(test)]
mod tests_;

pub use root_node_::{RootScope};
pub(crate) use tree_node_::TreeNodeBase;
pub(crate) use scope_node_::ScopeNode;

pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub const DEFAULT_CELL_SIZE: usize = core::mem::size_of::<usize>();
pub type PoolIndex = u16;

/// 默认 root 的发布点（指向 root 的 [`ScopeInner`]）；空指针表示尚未初始化。
pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<tree_node_::TreeNodeBase> =
    AtomicPtr::new(ptr::null_mut());
