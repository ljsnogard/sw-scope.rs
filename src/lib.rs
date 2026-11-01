#![feature(allocator_ext)]
#![feature(impl_trait_in_assoc_type)]
#![feature(ptr_metadata)]
#![feature(try_trait_v2)]

#![no_std]

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
mod test_support_;

#[cfg(test)]
mod demo_;

#[cfg(test)]
mod stress_tests_;

pub use abs_::{IntoEmplace, TrEmplace, TrScope};
pub use index_::{Owning, Retain, Sharing};
pub use scope_::{Scope, ScopeError};
pub use scope_inner_::{DefaultConfig, RootScope, TrScopeInnerConfig};

pub use scope_str_::ScopeStr;

pub mod x_deps {
    pub use atomic_sync;
    pub use atomic_sync::x_deps::{abs_cancel, abs_sync, atomex, funty};
    pub use bplustree_cmp;
    pub use gen_mcf2;

    pub use typeid;
}
