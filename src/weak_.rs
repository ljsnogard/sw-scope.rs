use core::{
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::{
    scope_inner_::{PoolIndex, ScopeInner},
    strong_::{StrongChunk, StrongChunkBase},
};

/// 其物理位置通常位于一个独立的、由 RootScope 管理、专门用于分配
/// weak_chunk 的内存块，因此不会有内存碎片。
#[repr(C)]
pub(crate) struct WeakChunk<T>
where
    T: ?Sized,
{
    /// 包含状态，以及 weak_count
    chunk_state_: WeakChunkState<T>,
    strong_chunk_: AtomicPtr<StrongChunkBase<T>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum DataState {
    /// The strong chunk is allocated and referenced only by the WeakChunk
    Allocated = 0x01,

    /// The strong chunk is owned by Retain<T>
    Retained = 0x02,

    /// The strong chunk is shared by Shared<T>
    Shared = 0x03,

    /// The data in strong chunk is destroyed
    Destroyed = 0x04,

    /// The memory of strong chunk is reclaimed
    Reclaimed = 0x05,
}

#[repr(C)]
struct WeakChunkState<T>
where
    T: ?Sized,
{
    /// 高4位用于存储 flag, 其中最高位用于表示锁状态，3位用于表示 DataState
    /// 剩余的位用于表示 Retain 的引用计数
    weak_count_: AtomicUsize,
    _unused_t_: PhantomData<NonNull<WeakChunk<T>>>,
}

pub(crate) struct WeakChunkStateGuard<'a, T>(&'a mut WeakChunkState<T>)
where
    T: ?Sized;

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> WeakChunk<T>
where
    T: ?Sized,
{
    const DATA_ST_MASK: usize = 0x07 << (usize::BITS - 4);
    const LOCK_ST_MASK: usize = 0x01 << (usize::BITS - 1);
    const WEAK_RC_MASK: usize = usize::MAX
        & (!Self::DATA_ST_MASK)
        & (!Self::LOCK_ST_MASK);

    #[inline]
    pub fn try_lock(&self, max_try: usize) -> Option<WeakChunkStateGuard<'_, T>> {
        self.chunk_state_.try_acq_state_guard(max_try)
    }

    #[inline]
    pub fn data_state(&self) -> DataState {
        self.chunk_state_.data_state()
    }

    pub fn incr_use_count(&self) -> usize {
        let s = self
            .chunk_state_
            .weak_count_
            .fetch_add(1, Ordering::AcqRel);
        s & Self::WEAK_RC_MASK
    }

    pub fn decr_use_count(&self) -> usize {
        let s = self
            .chunk_state_
            .weak_count_
            .fetch_sub(1, Ordering::AcqRel);
        s & Self::WEAK_RC_MASK
    }

    pub fn try_retain(&self) -> Result<NonNull<StrongChunk<T>>, DataState> {
        todo!()
    }

    pub fn try_share(&self) -> Result<NonNull<StrongChunk<T>>, DataState> {
        todo!()
    }
}

impl<T> WeakChunkState<T>
where
    T: ?Sized,
{
    pub const fn data_state(&self) -> DataState {
        todo!()
    }

    pub fn weak_count(&self) -> usize {
        todo!()
    }

    pub fn try_acq_state_guard(
        &self,
        max_try: usize,
    ) -> Option<WeakChunkStateGuard<T>> {
        todo!()
    }
}

impl<'a, T> core::ops::Deref for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    type Target = WeakChunk<T>;

    fn deref(&self) -> &Self::Target {
        let p = self.0 as *const WeakChunkState<T> as *const u8;
        unsafe { &*(p as *const WeakChunk<T>) }
    }
}


impl<'a, T> core::ops::DerefMut for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        let p = self.0 as *const WeakChunkState<T> as *const u8;
        unsafe { &mut *(p as *mut u8 as *mut WeakChunk<T>) }
    }
}

impl<'a, T> Drop for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    fn drop(&mut self) {
        todo!("restore the flag in WeakChunkState")
    }
}
