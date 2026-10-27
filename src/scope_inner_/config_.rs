use core::alloc::Allocator;

use crate::page_size_::TrPageSizeSelect;

pub(crate) trait TrScopeInnerConfig {
    type PoolAlloc: Allocator;
    type PageSize: TrPageSizeSelect;
}
