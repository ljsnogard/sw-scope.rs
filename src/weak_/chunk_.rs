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
    /// 在 WeakChunkPool 中的索引（0开始），也可用于定位 WeakChunkPool 地址。
    ///
    /// 这个序号只取决于槽位在池内数组中的位置，因此池在构造时就把全部槽位编号完毕，
    /// 此后终生只读：分配、归还都不再触碰它。既然是只读字段，就没有必要为它引入
    /// 原子或「只写一次」的接口。
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

    /// 读取槽位当前的弱引用计数。
    #[inline]
    pub fn weak_count(&self) -> usize {
        self.chunk_state_.weak_count()
    }

    #[inline]
    pub fn incr_use_count(&self) -> usize {
        self.chunk_state_.incr_use_count()
    }

    #[inline]
    pub fn decr_use_count(&self) -> usize {
        self.chunk_state_.decr_use_count()
    }

    /// 读取槽位在所属 `WeakChunkPool` 中的序号。该序号由池在构造时写死，总是等于槽位
    /// 在池内数组中的下标。
    #[inline]
    pub fn pool_order(&self) -> PoolIndex {
        self.chunk_state_.pool_order()
    }

    /// 读取空闲链表中的下一个槽位序号，仅供 `WeakChunkPool` 遍历空闲链使用。
    #[inline]
    pub fn next_freed(&self) -> PoolIndex {
        self.chunk_state_.next_freed()
    }

    /// 把槽位挂到空闲链表的表头：写入后继节点并清掉残留的弱引用计数。
    ///
    /// 仅供 `WeakChunkPool` 在归还槽位时调用。
    pub fn link_as_freed(&mut self, next: PoolIndex) {
        self.chunk_state_.set_next_freed(next);
        self.chunk_state_.reset_weak_state();
    }

    /// 初始化一段尚未投入使用的槽位数组，是 `WeakChunkPool` 构造期唯一的槽位入口。
    ///
    /// 完成三件事：
    /// - 按槽位在数组中的下标写死 `pool_order_`，该值此后终生只读，分配的代价里
    ///   不再包含任何序号写入；
    /// - 把链接串成初始空闲链：`i` 指向 `i + 1`，末位指向 `capacity`。这为池提供了
    ///   "只要未满就一定有表头可摘"的不变量，于是分配路径无须区分「从未分配过的槽位」
    ///   与「归还回来的槽位」；
    /// - 把弱引用计数清零。分配器返回的是未初始化内存，若不在此统一收口，槽位在首次
    ///   分配前就会带着随机内容被读到。
    ///
    /// # Panics
    ///
    /// 当 `slots.len()` 超出 [`PoolIndex`] 表示范围时 panic：链尾标记本身就是容量值，
    /// 它必须能放进一个 `PoolIndex`。
    pub fn init_slots(slots: &mut [Self], capacity: PoolIndex) {
        assert!(
            slots.len() == capacity as usize,
            "槽位数量({})必须与容量({})一致",
            slots.len(),
            capacity,
        );
        let last = slots.len() - 1;
        for (i, slot) in slots.iter_mut().enumerate() {
            let next = if i == last {
                capacity
            } else {
                (i + 1) as PoolIndex
            };
            let state = &mut slot.chunk_state_;
            state.pool_order_ = i as PoolIndex;
            state.set_next_freed(next);
            state.reset_weak_state();
        }
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

    /// 清除槽位上的弱引用计数，仅供 `WeakChunkPool` 在归还槽位时调用。
    ///
    /// 归还意味着该槽位上的弱引用已经结束，残留的计数只会让后续的读取者误判状态。
    pub fn reset_weak_state(&mut self) {
        self.weak_state_.store(0, Ordering::Release);
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
