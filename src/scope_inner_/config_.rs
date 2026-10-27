extern crate alloc;

use alloc::alloc::Global;
use core::{
    alloc::{Allocator, AllocatorClone},
    marker::PhantomData,
};

use crate::page_size::{self, TrPageSizeSelect};

pub trait TrScopeInnerConfig {
    type LockAlloc: AllocatorClone;
    type PageSize: TrPageSizeSelect;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultConfig<A = Global>(PhantomData<fn() -> A>)
where
    A: Allocator + Default;

impl<A> TrScopeInnerConfig for DefaultConfig<A>
where
    A: Allocator + Default,
{
    type LockAlloc = Global;
    type PageSize = page_size::PageSize2KB;
}
