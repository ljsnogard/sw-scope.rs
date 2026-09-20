#![feature(allocator_api)]
#![feature(generic_atomic)]

#![no_std]

mod abs_;

#[cfg(test)]
extern crate std;

mod index_;
mod scope_;
mod scope_inner_;
mod scope_str_;

mod strong_;
mod weak_;

#[cfg(test)]
mod demo_;

pub use abs_::TrScope;
pub use index_::{Owning, Sharing, Retain};
pub use scope_::{Scope, ScopeError};
pub use scope_str_::ScopeStr;
