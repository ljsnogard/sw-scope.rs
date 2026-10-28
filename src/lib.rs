#![feature(allocator_ext)]
#![feature(impl_trait_in_assoc_type)]
#![feature(ptr_metadata)]
#![feature(try_trait_v2)]
#![no_std]
// 本次重构先把新树结构编译通过：大量旧实现与辅助类型被停用但按约定保留在源码中，
// 因此暂时允许不可达代码。等 B+ 树操作与分配管线迁移完成、旧实现真正删除后，应移除
// 这个 allow。
#![allow(dead_code)]

extern crate alloc;

mod abs_;

#[cfg(any(test, feature = "std"))]
extern crate std;

mod index_;
pub mod page_size;
mod scope_;
mod scope_inner_;
mod scope_str_;
mod tunings_;

mod strong_;
mod weak_;

#[cfg(test)]
mod demo_;

#[cfg(test)]
mod stress_tests_;

pub use abs_::{IntoEmplace, TrEmplace, TrScope};
pub use index_::{Owning, Retain, Sharing};
pub use scope_::{Scope, ScopeError};
pub use scope_inner_::{DefaultConfig, TrScopeInnerConfig};

pub use scope_str_::ScopeStr;

pub mod x_deps {
    pub use atomic_sync;
    pub use atomic_sync::x_deps::{abs_cancel, abs_sync, atomex, funty};
    pub use bplustree_cmp;
    pub use gen_mcf2;

    pub use typeid;
}
