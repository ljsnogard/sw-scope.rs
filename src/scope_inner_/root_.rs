//! 整棵树的根：共享扩展、内联分配器与一次性初始化，以及由域定位 root。
//!
//! root 就是三者按顺序组合出来的一个分配块：
//!
//! ```text
//! RootScope<A, CELL_SIZE> {
//!     scope_inner_: ScopeInner<CELL_SIZE>,    // root 自己的域状态（与普通域同型）
//!     extension_:   RootExtension<CELL_SIZE>, // root 比普通域多出来的共享内容
//!     allocator_:   A,                        // 具体分配器，永远放最后
//! }
//! ```
//!
//! 各域只保存一个 `&'static dyn Allocator`，指向 root 的 `allocator_` 字段。只要分配器字段
//! 之前没有对齐填充，回退
//! `size_of::<ScopeInner>() + size_of::<RootExtension>()` 就回到 root 域，回退
//! `size_of::<RootExtension>()` 就得到扩展。初始化时会断言这一点（见
//! [`RootScope::try_init_default_root_scope_`]）。

use core::{
    alloc::{Allocator, Layout},
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use bplustree_cmp::BPlusTree;

use crate::{
    weak_::{PreDropRegistry, WeakSlot, WeakPool},
};

use super::{PoolIndex, ScopeInner};

const TREE_M: usize = 16;
type ScopeKey = usize;

pub(crate) struct RootScope {
    scopes_: BPlusTree<TREE_M, ScopeKey, NonNull<ScopeInner<>>
}
