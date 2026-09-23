//! 域的表示（[`ScopeNode`]）与整棵树的根（[`RootScope`]）。
//!
//! 拆分依据是"紧密共享同一组字段的操作放在一起"：
//!
//! - 本文件：常量、[`PoolIndex`]、默认 root 的发布点与逐项导出；
//! - [`tree_node_`]：所有域节点共享的父子 / 兄弟链（代际结构）；
//! - [`scope_node_`]：用户 `Scope` 的扩展字段（标志、子链、强池链、存活链、分配器引用）
//!   与强池 bump 分配管线；
//! - [`root_node_`]：整棵树的根——共享扩展（弱池链 + 类型级 `PreDrop` 注册表）、内联分配器、
//!   一次性初始化，以及由域定位 root；
//! - [`teardown_`]：清盘方案 A 的关闭 / 静默判定 / 待回收名单（清盘任务的跨线程设计见
//!   `dev-notes` 的模块综述）。
//!
//! 子模块是 `mod.rs` 的后代，因此可以直接访问节点的私有字段；模块外一律通过关联函数访问。

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
