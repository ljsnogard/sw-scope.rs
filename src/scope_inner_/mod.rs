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

use core::{
    alloc::Allocator,
    ptr::{self, NonNull},
    sync::atomic::{AtomicUsize, AtomicPtr}
};

use crate::strong_::StrongPool;
use crate::weak_::WeakChunk;

mod alloc_;
mod life_;
mod root_;
mod teardown_;

#[cfg(test)]
mod tests_;

pub(crate) use root_::RootScope;
pub(crate) use root_::RootExtension;

pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub const DEFAULT_CELL_SIZE: usize = core::mem::size_of::<usize>();
pub type PoolIndex = u16;

/// 默认 root 的发布点（指向 root 的 [`ScopeInner`]）；空指针表示尚未初始化。
pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<ScopeInner<DEFAULT_CELL_SIZE>> =
    AtomicPtr::new(ptr::null_mut());

/// 一个域的内部表示。
///
/// 域持有若干 [`StrongPool`] 组成的链，池内的对象位置终身不变（见 `StrongPool` 的文档）；
/// 域静默时整条链一次性回收。
///
/// 数据对象的"存活链"挂在弱槽位上（`WeakChunk::prev_live_` / `next_live_`），这里只保存
/// 链头与链尾。整棵树共享的东西——弱槽位池链、类型级 `PreDrop` 注册表——都属于
/// [`RootExtension`]（[`RootScope`] 中与分配器无关的那部分），本结构只保存分配器指针：
/// 它既用于分配，也用于**反向定位 root**（见 [`ScopeInner::root_`]）。
#[repr(C)]
pub struct ScopeInner<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> {
    /// 域标志位：`CLOSED` / `HANDLE_DROPPED`（取代原先的"子域数量"计数）
    flags_: AtomicUsize,
    /// 父域指针
    parent_scope_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 同父下的兄弟双向链
    prev_sibling_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    next_sibling_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 自己派生的子域双向链
    first_child_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    last_child_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 整棵树共享的"待回收名单"（只在 root 上有意义）：域被关闭后挂到这里，静默即可回收
    pending_head_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    pending_tail_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 名单内下一个；只在域已被关闭、挂在名单上时有效
    pending_next_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 指向 Scope 所持有的第一块内存池
    chain_head_: Option<NonNull<StrongPool<CELL_SIZE>>>,
    /// 指向 Scope 所持有的最新一块内存池
    chain_tail_: Option<NonNull<StrongPool<CELL_SIZE>>>,
    /// 存活链链头（弱槽位）
    live_head_: Option<NonNull<WeakChunk<()>>>,
    /// 存活链链尾（弱槽位）
    live_tail_: Option<NonNull<WeakChunk<()>>>,
    /// 指向 root 中内联的具体分配器；既用于分配，也用于反推 [`RootExtension`] 的基址。
    allocator_: &'static dyn Allocator,
}
