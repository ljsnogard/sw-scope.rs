use core::{
    mem,
    ptr::{self, NonNull},
    sync::atomic::Atomic,
};

use crate::{
    scope_inner_::PoolIndex,
    weak_::{DataState, WeakChunk},
};

/// 通常从 Scope 的首端开始分配
pub(crate) struct StrongChunk<T>
where
    T: ?Sized,
{
    base_: StrongChunkBase<T>,
    data_: T,
}


#[repr(C)]
pub(crate) struct StrongChunkBase<T>
where
    T: ?Sized,
{
    /// 包含状态，以及 strong_count
    chunk_state_: StrongChunkState,
    weak_chunk_: NonNull<WeakChunk<T>>,
}

#[repr(C)]
struct StrongChunkState {
    /// ChainedPool 内存池距离此地址有多少个 cell
    pool_: PoolIndex,
    /// 上一个 StrongChunk 距离本 chunk 有多少个 cell
    prev_: PoolIndex,
    /// 引用计数（仅Sharing<T> 时有效）
    refc_: Atomic<u32>,
}

impl<T> StrongChunk<T>
where
    T: ?Sized,
{
    #[inline]
    pub fn is_data_alive(&self) -> bool {
        self.base_.chunk_state_.is_data_alive()
    }

    #[inline]
    pub fn strong_count(&self) -> usize {
        self.base_.chunk_state_.strong_count()
    }

    pub const fn data_addr(&self) -> *mut u8 {
        let offset = mem::size_of_val(self);
        let p = self as *const Self as *const u8 as *mut u8;
        unsafe { p.byte_add(offset) }
    }

    pub fn try_get_data(&self) -> Option<&T> {
        todo!()
    }

    pub fn try_get_data_mut(&mut self) -> Option<&mut T> {
        todo!()
    }
}

impl StrongChunkState {
    pub fn strong_count(&self) -> usize {
        todo!()
    }

    pub fn is_data_alive(&self) -> bool {
        todo!()
    }
}
