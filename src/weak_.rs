use core::{
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{Atomic, AtomicPtr, AtomicU32, Ordering},
};

use crate::{scope_inner_::PoolIndex, strong_::{StrongChunk, StrongChunkBase}};

/// 其物理位置通常位于一个独立的、由 RootScope 管理、专门用于分配
/// weak_chunk 的内存块，因此不会有内存碎片。
#[repr(C)]
pub(crate) struct WeakChunk<T>
where
    T: ?Sized,
{
    /// 包含状态，以及 weak_count
    chunk_state_: WeakChunkState,
    strong_chunk_: AtomicPtr<StrongChunkBase>,
    _unused_t_: PhantomData<NonNull<StrongChunk<T>>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum DataState {
    /// The memory of strong chunk is reclaimed/unused. This must be the
    /// default value of DataState when initialized all bits should be 0.
    Reclaimed = 0x00,

    /// The strong chunk is allocated and referenced only by the WeakChunk
    Created = 0x01,

    /// The strong chunk is owned by Owning<T>
    Owning = 0x02,

    /// The strong chunk is shared by Sharing<T>
    Sharing = 0x03,

    /// The data in strong chunk is destroyed but memory not yet reclaimed
    Destroyed = 0x04,
}

#[repr(C)]
pub(crate) struct WeakChunkState {
    /// 在 WeakChunkPool 中的索引（0开始），也可用于定位 WeakChunkPool 地址
    pool_order_: PoolIndex,

    /// 协助 WeakChunPool 记录下一个空闲 Chunk
    next_freed_: PoolIndex,

    /// 最高位表示锁状态，接下来高3位表示 DataStae，剩余的表示引用计数
    weak_state_: AtomicU32,
}

pub(crate) struct WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    state_mut_: core::cell::UnsafeCell<&'a WeakChunkState>,
    _unused_t_: PhantomData<&'a mut WeakChunk<T>>,
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> WeakChunk<T>
where
    T: ?Sized,
{
    #[inline]
    pub fn try_lock(&self, max_try: usize) -> Option<WeakChunkStateGuard<'_, T>> {
        self.chunk_state_.try_acq_state_guard(max_try)
    }

    #[inline]
    pub fn data_state(&self) -> DataState {
        self.chunk_state_.data_state()
    }

    #[inline]
    pub fn incr_use_count(&self) -> usize {
        self.chunk_state_.incr_use_count()
    }

    #[inline]
    pub fn decr_use_count(&self) -> usize {
        self.chunk_state_.decr_use_count()
    }

    pub fn try_retain(&self) -> Result<NonNull<StrongChunk<T>>, DataState> {
        todo!()
    }

    pub fn try_share(&self) -> Result<NonNull<StrongChunk<T>>, DataState> {
        todo!()
    }
}

impl DataState {
    pub const fn new(v: u8) -> Self {
        match v {
            0x00 => DataState::Reclaimed,
            0x01 => DataState::Created,
            0x02 => DataState::Owning,
            0x03 => DataState::Sharing,
            0x04 => DataState::Destroyed,
            _ => unreachable!()
        }
    }
}

impl WeakChunkState {
    const DATA_ST_MASK: u32 = 0x7000_0000;
    const LOCK_ST_MASK: u32 = 0x8000_0000;
    const WEAK_RC_MASK: u32 = 0x0FFF_FFFF;

    pub fn data_state(&self) -> DataState {
        let s = self.weak_state_.load(Ordering::SeqCst);
        let s = (s & Self::DATA_ST_MASK) >> 28;
        DataState::new(s as u8)
    }

    pub fn weak_count(&self) -> usize {
        let s = self.weak_state_.load(Ordering::SeqCst);
        (s & Self::WEAK_RC_MASK) as usize
    }

    pub fn incr_use_count(&self) -> usize {
        let s = self
            .weak_state_
            .fetch_add(1, Ordering::AcqRel);
        (s & Self::WEAK_RC_MASK) as usize
    }

    pub fn decr_use_count(&self) -> usize {
        let s = self
            .weak_state_
            .fetch_sub(1, Ordering::AcqRel);
        (s & Self::WEAK_RC_MASK) as usize
    }

    pub fn try_acq_state_guard<T: ?Sized>(
        &self,
        max_try: usize,
    ) -> Option<WeakChunkStateGuard<'_, T>> {
        let mut c = 0usize;
        let mut current = 0;
        let success = Ordering::AcqRel;
        let failure = Ordering::Relaxed;
        loop {
            let new = current | Self::LOCK_ST_MASK;
            let x = self
                .weak_state_
                .compare_exchange_weak(current, new, success, failure);
            if let Result::Err(x) = x {
                if x & Self::LOCK_ST_MASK == Self::LOCK_ST_MASK {
                    return Option::None;
                }
                current = x;
            } else {
                break;
            }
            c += 1usize;
            if c >= max_try {
                return Option::None;
            }
        }
        Option::Some(WeakChunkStateGuard::new(self))
    }

    pub fn pool_order(&self) -> PoolIndex {
        self.pool_order_
    }

    pub fn next_freed(&self) -> PoolIndex {
        self.next_freed_
    }

    pub fn set_next_freed(&mut self, v: PoolIndex) {
        self.next_freed_ = v
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<'a, T> WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    const fn new(s: &'a WeakChunkState) -> Self {
        WeakChunkStateGuard {
            state_mut_: core::cell::UnsafeCell::new(s),
            _unused_t_: PhantomData,
        }
    }
}

impl<'a, T> core::ops::Deref for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    type Target = WeakChunk<T>;

    fn deref(&self) -> &Self::Target {
        let p = self.state_mut_.get() as *mut u8;
        unsafe { &*(p as *mut WeakChunk<T>) }
    }
}


impl<'a, T> core::ops::DerefMut for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        let p = self.state_mut_.get() as *mut u8;
        unsafe { &mut *(p as *mut WeakChunk<T>) }
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
