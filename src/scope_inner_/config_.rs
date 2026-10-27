extern crate alloc;

use alloc::alloc::Global;
use core::{
    alloc::{Allocator, AllocatorClone},
    marker::PhantomData,
};

use crate::page_size::{self, TrPageSizeSelect};

pub trait TrScopeInnerConfig {
    /// 每域一份的池分配器类型；实体尾部会内联存放它的一个值。
    type PoolAlloc: Allocator + Default + Send + Sync + 'static;

    /// 锁内核（等待队列 / `Arc<RwCore>`）使用的分配器类型。
    ///
    /// TODO(实验): 当前锁类型固定为 `&'static dyn Allocator`，实际使用 root 的分配器；
    /// 这个关联类型尚未接入。
    type LockAlloc: AllocatorClone;

    type PageSize: TrPageSizeSelect;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultConfig<A = Global>(PhantomData<fn() -> A>)
where
    A: Allocator + Default;

impl<A> TrScopeInnerConfig for DefaultConfig<A>
where
    A: Allocator + Default + Send + Sync + 'static,
{
    type PoolAlloc = A;
    type LockAlloc = Global;
    type PageSize = page_size::PageSize2KB;
}
