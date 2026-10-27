// 旧实现需要 `Allocator` 的 `allocate`/`deallocate` 扩展；迁移后暂未使用。
// #![feature(allocator_ext)]
#![feature(ptr_metadata)]
#![feature(try_trait_v2)]

#![no_std]
// 本次重构先把新树结构编译通过：大量旧实现与辅助类型被停用但按约定保留在源码中，
// 因此暂时允许不可达代码。等 B+ 树操作与分配管线迁移完成、旧实现真正删除后，应移除
// 这个 allow。
#![allow(dead_code)]

mod abs_;

#[cfg(any(test, feature = "std"))]
extern crate std;

mod index_;
mod page_size_;
mod scope_;
mod scope_inner_;
mod scope_str_;
mod tunings_;

mod strong_;
mod weak_;

#[cfg(test)]
mod demo_;

pub use abs_::{TrEmplace, TrScope, IntoEmplace};
pub use index_::{Owning, Sharing, Retain};
pub use scope_::{Scope, ScopeError};
// 新设计里 root 是 B+ 树的持有者，`Scope::root` 需要在公开签名中引用它。
pub use scope_inner_::RootScope;
pub use scope_str_::ScopeStr;

pub mod x_deps {
    pub use atomic_sync;
    pub use atomic_sync::x_deps::{abs_cancel, abs_sync, funty};
    pub use bplustree_cmp;
    pub use gen_mcf2;
}
