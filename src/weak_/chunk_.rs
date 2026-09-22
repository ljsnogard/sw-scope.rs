use core::{
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use crate::{
    atomic_::{MsbAsMutexSignal, SpinFlag},
    scope_inner_::PoolIndex,
    strong_::{StrongChunk, StrongChunkBase},
};

use super::pre_drop_::PreDropRecord;

/// 弱引用槽位。它既是 `Retain<T>` 的持有对象，也是**对象的身份与生命周期上下文**。
///
/// # 为什么它是最长寿的一环
///
/// `Retain<T>` 是类似 GC Handle 的东西：句柄传递到哪里，生命周期便带到哪里。而
/// `Owning<'a, T>` / `Sharing<'a, T>` 都是从 `Retain` 上"借"出来的伴生视图，它们的 `'a`
/// 绑定在 `&Retain` 上，**不可能比产生它们的那个 `Retain` 活得更久**。因此：
///
/// > 数据还活着 ⇒ 至少有一个 `Retain` 活着 ⇒ 这个 `WeakChunk` 还活着。
///
/// 这条不等式是后续所有设计的地基：
/// - 存活链的链接、清理登记都可以安全地放在弱槽位里——强块的数据区活着的整个期间，
///   弱槽位都不可能被回收复用；
/// - `?Sized` 的析构也因此可行：重建胖指针所需的元数据有地方长期保存。
///
/// # 何时可以被回收
///
/// **不是** `weak_count` 归零就算数。槽位的回收要综合 [`WeakChunkState::weak_state_`]
/// 里的状态与两个计数判断：当"没有任何 `Retain`"且"数据已经析构（或从未需要析构）"时，
/// 槽位才归还给 [`crate::weak_::WeakPool`]。
///
/// # 布局
///
/// 弱槽位与强块分属两个池，这里是弱池里的数组元素。槽位尺寸与 `T` 无关：`T` 只出现在
/// [`PhantomData`] 中，因此池可以统一按 `WeakChunk<()>` 的步长定位槽位。
#[repr(C)]
pub(crate) struct WeakChunk<T>
where
    T: ?Sized,
{
    /// 状态、弱计数与空闲链链接。
    chunk_state_: WeakChunkState,
    /// 存活链的链头方向链接（指向上一个存活对象）；这是"存活链挂弱槽位"的落点。
    prev_live_: AtomicPtr<WeakChunk<()>>,
    /// 存活链的链尾方向链接。
    next_live_: AtomicPtr<WeakChunk<()>>,
    /// 本槽位对应的强块。数据的析构与访问都要经过它。
    strong_chunk_: AtomicPtr<StrongChunkBase>,
    /// 类型擦除的清理登记，指向**每类型一份**的 [`PreDropRecord`]；空闲槽位为 [`None`]。
    ///
    /// 记录里不含数据区地址：它由 `strong_chunk_` 首址加上单态化入口自己算出的偏移重建。
    record_: Option<&'static PreDropRecord>,
    /// `T: ?Sized` 的类型元数据（切片长度 / 虚表）；`Sized` 时为空。
    ///
    /// 这是**逐对象**信息，所以不能进"每类型一份"的记录里，只能留在槽位上。
    meta_: *const (),
    /// `T` 只在类型上区分，不参与布局。
    _unused_t_: PhantomData<NonNull<StrongChunk<T>>>,
}

/// `WeakChunk` 的生命周期状态。**这是"数据要不要析构"的唯一权威来源**，
/// `StrongChunk` 侧不再有对应的状态枚举。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub(crate) enum DataState {
    /// 内存已回收，或槽位尚未分配。全零位是默认值。
    Reclaimed = 0x00,

    /// 强块已分配并持有数据，但当前没有任何强引用持有者（只被 `Retain` 句柄挂着）。
    Created = 0x01,

    /// 数据被 `Owning<T>` 独占持有。
    Owning = 0x02,

    /// 数据被 `Sharing<T>` 共享持有。
    Sharing = 0x03,

    /// 已有线程认领销毁，但数据的 `Drop` 尚未执行完毕。
    Destroying = 0x04,

    /// 数据已析构，但槽位与强块内存尚未回收。
    Destroyed = 0x05,

    /// 已退出存活链、弱引用计数归零，可随所属池一并回收。
    ///
    /// 这是"已纳入 GC 队列"的终态：存活链本身就是待清理队列的实体，状态位只负责
    /// 记录"这块钱销毁到哪一步"。
    Finalized = 0x06,
}

impl DataState {
    /// 数据是否仍然活着，即是否还需要一次析构。
    ///
    /// 语义是"数据尚未被销毁"，**不是**"还有强引用"：只剩 `Retain`（弱计数 ≥ 1、无强引用）
    /// 的块依然 alive，因此仍可被升级。
    #[inline]
    pub const fn is_data_alive(self) -> bool {
        matches!(
            self,
            DataState::Created | DataState::Owning | DataState::Sharing
        )
    }

    /// 该状态是否还能被认领销毁。
    #[inline]
    pub const fn is_claimable(self) -> bool {
        self.is_data_alive()
    }

    pub const fn new(v: u8) -> Self {
        match v {
            0x00 => DataState::Reclaimed,
            0x01 => DataState::Created,
            0x02 => DataState::Owning,
            0x03 => DataState::Sharing,
            0x04 => DataState::Destroying,
            0x05 => DataState::Destroyed,
            0x06 => DataState::Finalized,
            _ => unreachable!(),
        }
    }
}

/// 状态字使用的锁信号策略：最高位。
type LockSignal = MsbAsMutexSignal<u32>;

/// 状态字：锁位由 [`LockSignal`] 给出，其余位是状态与弱计数。
type StateWord = SpinFlag<u32, AtomicU32, LockSignal>;

/// 状态字的位布局：高 1 位锁、次 3 位 [`DataState`]、低 28 位**弱**引用计数。
///
/// 强引用计数**不在这里**，它由 [`StrongChunkBase`] 自己管理：两者数的是完全不同的东西。
#[repr(C)]
pub(crate) struct WeakChunkState {
    /// 在所属 `WeakPool` 中的索引（0 开始），也用于从槽位反推池地址。
    ///
    /// 只取决于槽位在池内数组中的位置，池在构造时一次写死，此后终生只读。
    pool_order_: PoolIndex,

    /// 协助 `WeakPool` 串联空闲槽位。
    next_freed_: PoolIndex,

    /// 状态字；锁位、状态位与弱计数的读写全部经由 [`SpinFlag`] 的操作。
    weak_state_: StateWord,
}

/// 把状态与计数打包成一个字。
const fn pack_(state: DataState, count: u32) -> u32 {
    ((state as u32) << WeakChunkState::DATA_SHIFT) | (count & WeakChunkState::WEAK_RC_MASK)
}

impl WeakChunkState {
    const DATA_ST_MASK: u32 = 0x7000_0000;
    const WEAK_RC_MASK: u32 = 0x0FFF_FFFF;
    const DATA_SHIFT: u32 = 28;

    /// 槽位尚未被分配给任何对象时的初值。
    ///
    /// 因为 [`SpinFlag::new`] 要构造原子字，所以本函数不是 `const fn`。
    pub(crate) fn empty_() -> Self {
        WeakChunkState {
            pool_order_: 0,
            next_freed_: 0,
            weak_state_: SpinFlag::new(0),
        }
    }

    // -- 锁：语义操作由 `SpinFlag` 提供，CAS 不出现在本类型里 -----------------

    /// 抢占状态锁：以最高位为锁信号，`max_try == 0` 表示忙等。
    ///
    /// **私有**：锁的释放只允许发生在 [`WeakChunkStateGuard`] 的 `Drop` 里，因此不提供
    /// 任何"手动解锁"入口，也不允许外部单独抢锁——否则就会出现"锁被抢到、却没人负责
    /// 释放"的状态。
    fn acquire_(&self, max_try: usize) -> bool {
        self.weak_state_.try_acquire(max_try)
    }

    /// 释放状态锁。**只应由 [`WeakChunkStateGuard`] 的 `Drop` 调用。**
    fn release_lock_(&self) {
        let _ = self.weak_state_.release();
    }

    // -- 读写 ---------------------------------------------------------------

    /// 从状态字里取出 [`DataState`]。
    #[inline]
    fn state_of_(raw: u32) -> DataState {
        DataState::new(((raw & Self::DATA_ST_MASK) >> Self::DATA_SHIFT) as u8)
    }

    #[inline]
    pub(crate) fn data_state(&self) -> DataState {
        Self::state_of_(self.weak_state_.read())
    }

    /// 弱引用计数。
    #[inline]
    pub(crate) fn weak_count(&self) -> usize {
        (self.weak_state_.read() & Self::WEAK_RC_MASK) as usize
    }

    #[inline]
    pub(crate) fn incr_weak_count(&self) -> usize {
        let s = self.weak_state_.fetch_add(1);
        ((s & Self::WEAK_RC_MASK) + 1) as usize
    }

    #[inline]
    pub(crate) fn decr_weak_count(&self) -> usize {
        let s = self.weak_state_.fetch_sub(1);
        ((s & Self::WEAK_RC_MASK) - 1) as usize
    }

    /// 是否处于加锁状态。
    #[inline]
    pub(crate) fn is_locked(&self) -> bool {
        self.weak_state_.is_locked()
    }

    // -- 状态迁移：CAS 隐藏在 `SpinFlag::try_update` 里 ----------------------

    /// 把字里的状态位换成 `to`，保持锁位与弱计数不变。
    fn with_state_(raw: u32, to: DataState) -> u32 {
        (raw & !Self::DATA_ST_MASK) | ((to as u32) << Self::DATA_SHIFT)
    }

    /// 按状态位做一次条件迁移（无锁 CAS，锁位不受影响）。
    ///
    /// `desire` 返回 [`None`] 表示"不满足前提"，直接放弃；返回的目标状态与当前一致时视为
    /// 已经就位，同样返回当前状态。
    fn transition_<F>(&self, mut desire: F) -> Option<DataState>
    where
        F: FnMut(DataState) -> Option<DataState>,
    {
        self.weak_state_.try_update(|raw| {
            let from = Self::state_of_(raw);
            let to = desire(from)?;
            if to == from {
                return Option::Some((raw, from));
            }
            Option::Some((Self::with_state_(raw, to), from))
        })
    }

    /// 仅当状态位当前为 `from` 时把它迁到 `to`，保持锁位与弱计数不变。
    ///
    /// 只有**真正由本次调用完成迁移**才返回 `Some`，"状态本来就已经是 `to`" 不算成功
    /// ——升级路径要靠这个返回值判断自己是不是赢家。
    pub(crate) fn try_transition_state(
        &self,
        from: DataState,
        to: DataState,
    ) -> Option<DataState> {
        self.transition_(|current| (current == from).then_some(to))
    }

    /// 认领销毁：把状态从 `{Created, Owning, Sharing}` 搬到 [`DataState::Destroying`]。
    ///
    /// 返回先前状态表示认领成功；返回 [`None`] 表示已有别的路径处理过（或该块从未携带
    /// 存活数据），调用方必须放弃析构。这是在"多路径析构"下保证**恰好销毁一次**的唯一
    /// 裁决点：`Owning::drop`、`Sharing::drop` 的最后一次减计数、以及清盘兜底，都只能
    /// 通过这里竞争。
    pub(crate) fn try_claim_destroy(&self) -> Option<DataState> {
        self.transition_(|from| {
            from.is_claimable().then_some(DataState::Destroying)
        })
    }

    /// 标记数据已析构完成。只有成功认领过销毁的线程应当调用。
    pub(crate) fn mark_destroyed(&self) {
        let _ = self.transition_(|from| (from == DataState::Destroying).then_some(DataState::Destroyed));
    }

    /// 标记已退出存活链、可随池回收。只有弱引用计数归零后才应调用。
    pub(crate) fn mark_finalized(&self) {
        let _ = self.transition_(|_| Option::Some(DataState::Finalized));
    }

    /// 把状态置为 `Created`。分配路径在数据就位、槽位正式投入使用之后调用。
    pub(crate) fn init_created(&self) {
        let _ = self.transition_(|_| Option::Some(DataState::Created));
    }

    /// 只在当前为 `Created` 时把状态置为 `to`；返回先前状态。
    ///
    /// 这个条件迁移同时充当"无强引用"这一前提的判据与上锁动作，因此不需要额外的锁位。
    pub(crate) fn try_set_state(&self, to: DataState) -> Option<DataState> {
        self.transition_(|from| (from == DataState::Created).then_some(to))
    }

    /// 把状态无条件搬到 `to`（保持锁位与两个计数不变）。
    pub(crate) fn mark_state(&self, to: DataState) {
        let _ = self
            .weak_state_
            .try_update(|raw| Option::Some((Self::with_state_(raw, to), ())));
    }

    pub(crate) fn pool_order(&self) -> PoolIndex {
        self.pool_order_
    }

    pub(crate) fn next_freed(&self) -> PoolIndex {
        self.next_freed_
    }

    pub(crate) fn set_next_freed(&mut self, v: PoolIndex) {
        self.next_freed_ = v
    }

    /// 归还槽位时把状态字重置为"未分配"初值。
    ///
    /// 仅供 `WeakPool` 在归还槽位时调用；分配器给的是未初始化内存，**首次**初始化槽位
    /// 必须走 [`WeakChunkState::init_unallocated`]，否则会对随机内容做断言。
    pub(crate) fn reset_all(&mut self) {
        // 归还时还锁着，说明有线程卡在升级/初始化路径里，这属于逻辑错误
        debug_assert!(!self.is_locked(), "归还槽位时状态锁应当已经释放");
        self.weak_state_ = SpinFlag::new(0);
    }

    /// 首次初始化一个从未使用过的槽位的状态字：无条件写入初值。
    ///
    /// 与 [`WeakChunkState::reset_all`] 的区别是这里不做任何断言——此刻内存里是分配器
    /// 给的随机字节，没有"上一个使用者"可言。
    pub(crate) fn init_unallocated(&mut self) {
        self.weak_state_ = SpinFlag::new(0);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> WeakChunk<T>
where
    T: ?Sized,
{
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
    pub fn incr_weak_count(&self) -> usize {
        self.chunk_state_.incr_weak_count()
    }

    #[inline]
    pub fn decr_weak_count(&self) -> usize {
        self.chunk_state_.decr_weak_count()
    }

    /// 直接抬高弱引用计数，仅供测试构造"还有 `Retain` 在世"的场景。
    #[cfg(test)]
    pub fn incr_use_count(&self) -> usize {
        self.chunk_state_.incr_weak_count()
    }

    #[inline]
    pub fn pool_order(&self) -> PoolIndex {
        self.chunk_state_.pool_order()
    }

    #[inline]
    pub fn next_freed(&self) -> PoolIndex {
        self.chunk_state_.next_freed()
    }

    // -- 状态锁 -------------------------------------------------------------

    /// 尝试获取本槽位的状态锁，最多自旋 `max_try` 次。
    ///
    /// 成功返回守卫；竞争者持有锁时返回 [`None`]。这是"尝试性"语义，不做阻塞等待。
    /// 锁的释放只由守卫的析构负责。
    pub fn try_lock(&self, max_try: usize) -> Option<WeakChunkStateGuard<'_, T>> {
        WeakChunkStateGuard::try_acquire_(self, max_try)
    }

    /// [`WeakChunk::try_lock`] 的别名，与状态层上的命名保持对应。
    #[inline]
    pub fn try_acq_state_guard(&self, max_try: usize) -> Option<WeakChunkStateGuard<'_, T>> {
        self.try_lock(max_try)
    }

    /// 忙等直到拿到状态锁。
    ///
    /// 供确信锁会很快释放的阻塞式路径使用；锁总会在守卫析构时释放，因此不会永久自旋。
    pub fn lock_busy(&self) -> WeakChunkStateGuard<'_, T> {
        WeakChunkStateGuard::spin_acquire_(self)
    }

    // -- 强块与析构登记 -----------------------------------------------------

    /// 本槽位对应的强块；尚未分配强块时为空。
    #[inline]
    pub fn strong_chunk(&self) -> Option<NonNull<StrongChunkBase>> {
        NonNull::new(self.strong_chunk_.load(Ordering::Acquire))
    }

    /// 记录强块指针。
    #[inline]
    pub fn set_strong_chunk(&self, chunk: *mut StrongChunkBase) {
        self.strong_chunk_.store(chunk, Ordering::Release);
    }

    // -- 升级路径 -----------------------------------------------------------

    /// 尝试把 `Retain` 的持有关系升级为 `Owning`。
    ///
    /// 前提是当前状态恰为 [`DataState::Created`]（数据活着且没有强引用持有者）。状态位上的
    /// 迁移同时充当"无强引用"的判据与上锁动作，因此成败唯一、不需要额外的锁。
    ///
    /// # Errors
    ///
    /// 状态不是 `Created`（数据已析构，或已被 `Owning` / `Sharing` 持有）时返回当前状态。
    pub fn try_owning(&self) -> Result<NonNull<StrongChunkBase>, DataState> {
        let Some(chunk) = self.strong_chunk() else {
            return Result::Err(self.data_state());
        };
        match self
            .chunk_state_
            .try_transition_state(DataState::Created, DataState::Owning)
        {
            Option::Some(_) => Result::Ok(chunk),
            Option::None => Result::Err(self.data_state()),
        }
    }

    /// 尝试把 `Retain` 的持有关系升级为 `Sharing`。
    ///
    /// - 状态为 `Created`：置为 `Sharing` 并把强计数置 1；
    /// - 状态已经是 `Sharing`：只把强计数加 1。这条**幂等增量**是 README 使用思路里
    ///   "泄漏一个 `Sharing` 之后仍能继续共享"的依据。
    ///
    /// # Errors
    ///
    /// 数据已被 `Owning` 独占、或已被销毁 / 回收时返回当前状态。
    pub fn try_sharing(&self) -> Result<NonNull<StrongChunkBase>, DataState> {
        let Some(chunk) = self.strong_chunk() else {
            return Result::Err(self.data_state());
        };
        if self
            .chunk_state_
            .try_transition_state(DataState::Created, DataState::Sharing)
            .is_some()
        {
            // SAFETY: 强块与身份槽位互相绑定，chunk 有效
            unsafe { &*chunk.as_ptr() }.incr_strong_count();
            return Result::Ok(chunk);
        }
        if self.data_state() == DataState::Sharing {
            // SAFETY: 同上
            unsafe { &*chunk.as_ptr() }.incr_strong_count();
            return Result::Ok(chunk);
        }
        Result::Err(self.data_state())
    }

    /// 初始化一整段从未使用过的槽位，是 `WeakPool` 构造期唯一的槽位入口。
    ///
    /// - 按数组下标写死 `pool_order_`，此后终生只读；
    /// - 把空闲链接串成顺序链：`i` 指向 `i + 1`，末位指向 `capacity`，于是分配路径
    ///   无须区分「从未分配过的槽位」与「归还回来的槽位」；
    /// - 把状态、计数、存活链链接与清理登记一律拉到干净初值（分配器给的是未初始化内存）。
    ///
    /// # Panics
    ///
    /// `slots.len()` 与 `capacity` 不一致时 panic：链尾标记就是容量值，必须能放进
    /// 一个 [`PoolIndex`]。
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
            slot.chunk_state_.pool_order_ = i as PoolIndex;
            slot.chunk_state_.set_next_freed(next);
            slot.chunk_state_.init_unallocated();
            slot.set_strong_chunk(ptr::null_mut());
            slot.set_prev_live(ptr::null_mut());
            slot.set_next_live(ptr::null_mut());
            slot.record_ = Option::None;
            slot.meta_ = ptr::null();
        }
    }

    /// 槽位状态（锁、弱计数与空闲链链接）。
    #[inline]
    pub(crate) fn chunk_state(&self) -> &WeakChunkState {
        &self.chunk_state_
    }

    /// 槽位状态的可变引用，供弱池在分配 / 归还时改写空闲链。
    #[inline]
    pub(crate) fn chunk_state_mut(&mut self) -> &mut WeakChunkState {
        &mut self.chunk_state_
    }

    /// 类型擦除的清理登记；空闲槽位为 [`None`]。
    #[inline]
    pub(crate) fn record(&self) -> Option<&'static PreDropRecord> {
        self.record_
    }

    /// `?Sized` 类型元数据（切片长度 / 虚表）；`Sized` 时为空指针。
    #[inline]
    pub(crate) fn meta(&self) -> *const () {
        self.meta_
    }

    /// 造一个栈上的空槽位，仅供单元测试使用。
    #[cfg(test)]
    pub(crate) fn stack_empty_() -> Self {
        WeakChunk {
            chunk_state_: WeakChunkState::empty_(),
            prev_live_: AtomicPtr::new(ptr::null_mut()),
            next_live_: AtomicPtr::new(ptr::null_mut()),
            strong_chunk_: AtomicPtr::new(ptr::null_mut()),
            record_: Option::None,
            meta_: ptr::null(),
            _unused_t_: PhantomData,
        }
    }

    /// 登记类型擦除的清理信息：调用方已经算好了有效登记与 `?Sized` 元数据。
    ///
    /// 类型相关的调用方（例如 `StrongChunk`）走这个入口：在那里有具体的 `T`，可以算出
    /// 每类型一份的 [`PreDropRecord`] 与元数据。
    pub(crate) fn set_record_erased_(
        &mut self,
        record_: Option<&'static PreDropRecord>,
        meta_: *const (),
    ) {
        self.record_ = record_;
        self.meta_ = meta_;
    }

    /// 按登记执行清理：先跑 `PreDrop` 钩子，再析构数据。
    ///
    /// # Safety
    ///
    /// 必须先由状态机认领销毁，且恰好调用一次。
    pub(crate) unsafe fn drop_data(&self) {
        let Option::Some(record) = self.record_ else {
            return;
        };
        let base = self.strong_chunk_.load(Ordering::Acquire);
        debug_assert!(!base.is_null(), "清理时强块指针必须已经绑定");
        // SAFETY: 由调用方保证已认领且尚未析构；登记与元数据来自同一块数据
        unsafe { record.run_(base.cast::<u8>(), self.meta_) };
    }

    // -- 数据访问（按 U 重建，支持 ?Sized）---------------------------------

    /// 按 `U` 重建数据引用。调用方必须保证数据仍然活着。
    ///
    /// 数据区地址不再单独保存：由强块首址 + 单态化入口自己算出的偏移重建；`?Sized` 的元数据
    /// 则来自槽位自己的 `meta_`。
    ///
    /// # Safety
    ///
    /// 数据必须尚未析构，且 `U` 必须与当初分配的 `T` 是同一个类型。
    pub(crate) unsafe fn data_ref_as<U: ?Sized + ptr::Pointee>(&self) -> &U {
        let base = self.strong_chunk_.load(Ordering::Acquire);
        // SAFETY: 由调用方保证数据尚未析构；元数据来自同一次分配的登记
        let data = unsafe { data_ptr_of_::<U>(base.cast::<u8>(), self.meta_) };
        // SAFETY: 由调用方保证数据尚未析构
        unsafe { &*data }
    }

    /// 按 `U` 重建数据可变引用。
    ///
    /// # Safety
    ///
    /// 同 [`WeakChunk::data_ref_as`]，且调用方必须保证独占访问。
    pub(crate) unsafe fn data_ref_mut_as<U: ?Sized + ptr::Pointee>(&mut self) -> &mut U {
        let base = self.strong_chunk_.load(Ordering::Acquire);
        // SAFETY: 由调用方保证数据尚未析构且独占
        let data = unsafe { data_ptr_of_::<U>(base.cast::<u8>(), self.meta_) };
        // SAFETY: 由调用方保证数据尚未析构且独占
        unsafe { &mut *data }
    }

    // -- 存活链 -------------------------------------------------------------

    #[inline]
    pub(crate) fn next_live(&self) -> *mut WeakChunk<()> {
        self.next_live_.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn prev_live(&self) -> *mut WeakChunk<()> {
        self.prev_live_.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn set_next_live(&self, next: *mut WeakChunk<()>) {
        self.next_live_.store(next, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn set_prev_live(&self, prev: *mut WeakChunk<()>) {
        self.prev_live_.store(prev, Ordering::Relaxed);
    }

    // -- 池的维护 -----------------------------------------------------------

    /// 把槽位串成空闲链的一环，并清掉上一轮使用留下的状态与清理登记。
    ///
    /// 仅供 `WeakPool` 在归还槽位时调用。
    pub(crate) fn link_as_freed(&mut self, next: PoolIndex) {
        self.chunk_state_.set_next_freed(next);
        self.chunk_state_.reset_all();
        self.set_strong_chunk(ptr::null_mut());
        self.set_prev_live(ptr::null_mut());
        self.set_next_live(ptr::null_mut());
        self.record_ = Option::None;
        self.meta_ = ptr::null();
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 持有弱槽位状态锁的守卫。
///
/// # 它与状态 CAS 的分工
///
/// `weak_state_` 里的状态迁移 CAS 只保证**"状态迁移唯一"**；而升级/初始化路径要保护的
/// 是一组彼此相关的写：弱引用计数、`strong_chunk_`、清理登记 `record_`、以及强块的强计数。
/// 这些字段不在同一个字里，CAS 管不住它们的组合，于是需要本守卫把"读判据 + 一组写"
/// 合成一个临界区：
///
/// - `try_owning` / `try_sharing`：锁住之后才检查状态、绑定强块、写登记、抬计数；
/// - 分配路径：锁住之后才把"已就位的数据"正式置为 `Created`，避免外界看到半成品。
///
/// 本守卫 `Deref` 到 [`WeakChunk`]，因此可以在临界区内改动槽位字段。`Drop` 负责释放
/// 锁位——即使临界区里 panic，锁也一定被归还。
pub(crate) struct WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    /// 指向槽位内的状态字。持有它等于持锁。
    state_: NonNull<WeakChunkState>,
    /// 用于在 `Drop` 时找回所属槽位。
    owner_: NonNull<WeakChunk<T>>,
    /// 借用标记。
    _borrow: PhantomData<&'a WeakChunk<T>>,
}

impl<'a, T> WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    /// 尝试抢锁并交出守卫；抢不到返回 [`None`]。
    ///
    /// # Panic 安全性
    ///
    /// 唯一可能 panic 的步骤是抢锁本身，而它发生在"持有锁"之前。抢到锁之后再执行的只有
    /// 一次结构体字面量构造（指针运算 + `PhantomData`），既不分配也不调用用户代码，
    /// 因此不存在"锁已置位、却没人负责释放"的窗口。锁一旦置位，守卫就已经在返回路上。
    fn try_acquire_(owner: &'a WeakChunk<T>, max_try: usize) -> Option<Self> {
        let owner_ = NonNull::from(owner);
        // SAFETY: chunk_state_ 是 WeakChunk 的字段；只投影地址，不解引用
        let state_ = unsafe {
            NonNull::new_unchecked(
                ptr::addr_of!((*owner_.as_ptr()).chunk_state_) as *mut WeakChunkState
            )
        };
        // SAFETY: state_ 指向 owner 的状态字，owner 的生命周期覆盖返回值
        if unsafe { state_.as_ref() }.acquire_(max_try) {
            Option::Some(WeakChunkStateGuard {
                state_,
                owner_,
                _borrow: PhantomData,
            })
        } else {
            Option::None
        }
    }

    /// 忙等抢锁并交出守卫：`max_try == 0` 表示不限重试次数，因此必然成功。
    ///
    /// 与 [`WeakChunkStateGuard::try_acquire_`] 共用同一条抢锁路径，只是参数不同。
    fn spin_acquire_(owner: &'a WeakChunk<T>) -> Self {
        // 不限次数的抢锁不会失败，因此这里可以解包
        Self::try_acquire_(owner, 0).expect("不限次数的抢锁必定成功")
    }

    /// 本守卫持有的槽位。
    #[inline]
    pub(crate) fn chunk(&self) -> &'a WeakChunk<T> {
        // SAFETY: owner_ 来自调用方的借用，守卫的生命周期不超过它
        unsafe { self.owner_.as_ref() }
    }

    /// 在临界区内取可变引用。锁保证同一时刻只有一个守卫，因此独占成立。
    #[inline]
    pub(crate) fn chunk_mut(&mut self) -> &'a mut WeakChunk<T> {
        // SAFETY: 持锁即独占；守卫的 Drop 会释放锁
        unsafe { &mut *self.owner_.as_ptr() }
    }
}

impl<'a, T> core::ops::Deref for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    type Target = WeakChunk<T>;

    fn deref(&self) -> &Self::Target {
        self.chunk()
    }
}

impl<'a, T> core::ops::DerefMut for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.chunk_mut()
    }
}

impl<'a, T> Drop for WeakChunkStateGuard<'a, T>
where
    T: ?Sized,
{
    /// **解锁的唯一发生点。**
    ///
    /// 这里直接清锁位，不经过任何辅助方法：只要"解锁"还能被别处调用，就无法保证
    /// "持锁者一定负责释放"这条不变量。守卫存在即锁存在，守卫析构即锁消失。
    fn drop(&mut self) {
        // SAFETY: state_ 指向本守卫持有的那个槽位的状态字
        unsafe { self.state_.as_ref() }.release_lock_();
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 把 `T` 的类型元数据搬到裸指针宽度。
///
/// 元数据要么是 `()`（`Sized`）、要么是 `usize`（切片长度）、要么是虚表指针，
/// 三者都恰好占用一个机器字，因此可以按字节搬运。
pub(crate) fn meta_to_raw_<M>(meta: M) -> *const () {
    if mem::size_of::<M>() == 0 {
        // `T: Sized` 的元数据是 `()`，宽度为 0，直接给出空指针（也避免对空地址做拷贝）
        return ptr::null();
    }
    let mut out = mem::MaybeUninit::<*const ()>::uninit();
    // SAFETY: 元数据与裸指针同宽；目标是未初始化的 MaybeUninit，两段内存不重叠
    unsafe {
        ptr::copy_nonoverlapping(
            (&raw const meta).cast::<u8>(),
            out.as_mut_ptr().cast::<u8>(),
            mem::size_of::<*const ()>(),
        );
        out.assume_init()
    }
}

/// [`meta_to_raw_`] 的逆操作。
///
/// # Safety
///
/// `raw` 必须是用同一个 `T` 经 [`meta_to_raw_`] 得到的值。
pub(crate) unsafe fn raw_to_meta_<T: ?Sized + ptr::Pointee>(raw: *const ()) -> T::Metadata {
    if mem::size_of::<T::Metadata>() == 0 {
        // 与 meta_to_raw_ 对称：零宽元数据不必搬运
        return unsafe { mem::MaybeUninit::<T::Metadata>::zeroed().assume_init() };
    }
    let mut meta = mem::MaybeUninit::<T::Metadata>::uninit();
    // SAFETY: 由调用方保证宽度与来源类型一致；目标是未初始化的 MaybeUninit
    unsafe {
        ptr::copy_nonoverlapping(
            (&raw const raw).cast::<u8>(),
            meta.as_mut_ptr().cast::<u8>(),
            mem::size_of::<*const ()>(),
        );
        meta.assume_init()
    }
}

/// 读取弱槽位登记的 `?Sized` 类型元数据（`Sized` 时为空）。
pub(crate) fn meta_of_(weak: &WeakChunk<()>) -> *const () {
    weak.meta_
}

/// [`meta_of_`] 的逆操作：把裸元数据还原成 `T` 的元数据。
///
/// # Safety
///
/// `raw` 必须是用同一个 `T` 登记下来的值。
pub(crate) unsafe fn meta_from_raw_<T: ?Sized + ptr::Pointee>(raw: *const ()) -> T::Metadata {
    // SAFETY: 由调用方保证来源类型一致
    unsafe { raw_to_meta_::<T>(raw) }
}

/// 由"强块首址 + `?Sized` 元数据"还原数据区指针。
///
/// `StrongChunk<T>` 的 `base_` 在偏移 0，因此强块首址与 `StrongChunk<T>` 首址相同；数据区
/// 偏移交给编译器在这个单态化里算，所以清理登记里不必保存数据地址。
///
/// # Safety
///
/// `base` 必须是 `StrongChunk<T>` 的首地址，`meta` 必须来自同一次分配登记。
pub(crate) unsafe fn data_ptr_of_<T: ?Sized>(base: *mut u8, meta: *const ()) -> *mut T {
    let meta = unsafe { raw_to_meta_::<T>(meta) };
    // SAFETY: base 是 StrongChunk<T> 的首地址，meta 与之一致
    let chunk = ptr::from_raw_parts_mut::<StrongChunk<T>>(base.cast::<()>(), meta);
    // SAFETY: 同上
    unsafe { (*chunk).data_ptr() }
}
