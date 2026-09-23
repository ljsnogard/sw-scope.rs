#![feature(allocator_api)]
#![feature(coerce_unsized)]
#![feature(negative_impls)]
#![feature(impl_restriction)]
#![feature(ptr_metadata)]
#![feature(try_trait_v2)]
#![feature(unsize)]

#![no_std]

#[cfg(any(test, feature = "std"))]
extern crate std;

#[cfg(feature = "core-alloc")]
extern crate alloc;

mod abs_;
mod atomic_;
mod emplace_;
mod smart_pointer_;
mod scope_;
mod scope_tree_;
mod scope_str_;
mod share_marker_;
mod strong_;
mod weak_;

#[cfg(test)]
mod demo_;

pub use abs_::{TrPreDrop, TrScope, TrShareMarker};
pub use smart_pointer_::{Owning, Retain, Sharing};
pub use scope_::{Local, RootScope, Scope, ScopeError, Shared};
pub use scope_str_::ScopeStr;
pub use emplace_::{IntoEmplace, TrEmplace};
