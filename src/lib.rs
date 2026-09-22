#![feature(allocator_api)]
#![feature(coerce_unsized)]
#![feature(generic_atomic)]
#![feature(ptr_metadata)]
#![feature(try_trait_v2)]
#![feature(unsize)]

#![no_std]

mod abs_;
mod atomic_;

#[cfg(any(test, feature = "std"))]
extern crate std;

#[cfg(feature = "core-alloc")]
extern crate alloc;

mod index_;
mod scope_;
mod scope_inner_;
mod scope_str_;

mod strong_;
mod weak_;

#[cfg(test)]
mod demo_;

pub use abs_::{TrEmplace, TrScope, IntoEmplace};
pub use index_::{Owning, Sharing, Retain};
pub use scope_::{Scope, ScopeError};
pub use scope_str_::ScopeStr;
