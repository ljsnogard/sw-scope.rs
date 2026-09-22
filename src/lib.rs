#![feature(allocator_api)]
#![feature(coerce_unsized)]
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
mod index_;
mod scope_;
mod scope_inner_;
mod scope_str_;
mod share_marker_;
mod strong_;
mod weak_;

#[cfg(test)]
mod demo_;

pub use abs_::{TrScope, TrShareMarker};
pub use index_::{Owning, Sharing, Retain};
pub use scope_::{LocalScope, Scope, ScopeError, SharedScope};
pub use scope_str_::ScopeStr;
pub use emplace_::{IntoEmplace, TrEmplace};
