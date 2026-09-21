use core::{
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use crate::{
    scope_inner_::PoolIndex,
    strong_::{StrongChunk, StrongChunkBase},
};

/// 一次 CAS 的三种结局，语义对齐 `atomic_sync` 所用的 `atomex::CmpxchResult`。
///
/// 区分三者是为了让重试循环能判断"继续用旧值重试"还是"必须重新读状态"。本 crate 目前
/// 零依赖，因此就地定义这一小块，而不是引入外部 crate。
#[derive(Debug, Clone)]
pub(crate) enum CmpxchResult<T> {
    /// CAS 成功，携带比较时的旧值。
    Succ(T),
    /// 判据不成立，压根没发起 CAS；携带当前值。
    Unexpected(T),
    /// 判据成立但 CAS 失败——期间被别人改动过；携带观察到的当前值。
    Fail(T),
}

impl<T> CmpxchResult<T> {
    #[inline]
    pub(crate) const fn is_succ(&self) -> bool {
        matches!(self, CmpxchResult::Succ(_))
    }

    /// 成功时取出旧值。
    #[inline]
    pub(crate) fn succ(self) -> Option<T> {
        match self {
            CmpxchResult::Succ(t) => Option::Some(t),
            _ => Option::None,
        }
    }
}

/// 重建并析构某个 `T` 所需的元数据。
///
/// 弱槽位是**生命周期最长**的一环（见 [`WeakChunk`] 的文档），因此把"如何析构一块数据"
/// 这类用户日常访存不会接触的信息放在这里：既不占用 `StrongPool` 里的 arena 空间，
/// 也不会因为强块数据区消失而失效。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct DropVtable {
    /// 单态化的析构入口，接收"数据区裸地址 + 类型元数据"。
    ///
    /// 类型元数据在 `T: Sized` 时为空指针；`?Sized` 时是虚表指针或切片长度，
    /// 由入口内部还原成胖指针后 `drop_in_place`。
    pub(crate) drop_fn_: Option<unsafe fn(*mut u8, *const ())>,
    /// 数据区起址。
    pub(crate) data_: *mut u8,
    /// `T: ?Sized` 的类型元数据；`Sized` 时为空。
    pub(crate) meta_: *const (),
}

impl DropVtable {
    /// 空登记：表示该槽位还没有承载任何需要析构的数据。
    pub(crate) const fn empty_() -> Self {
        DropVtable {
            drop_fn_: Option::None,
            data_: ptr::null_mut(),
            meta_: ptr::null(),
        }
    }

    /// 登记一块需要（或不需要）析构的数据。
    pub(crate) const fn new_(
        drop_fn_: Option<unsafe fn(*mut u8, *const ())>,
        data_: *mut u8,
        meta_: *const (),
    ) -> Self {
        DropVtable {
            drop_fn_,
            data_,
            meta_,
        }
    }
}

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
/// - 存活链的链接、类型擦除析构的登记都可以安全地放在弱槽位里——强块的数据区活着的整个
///   期间，弱槽位都不可能被回收复用；
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
    pub(crate) chunk_state_: WeakChunkState,
    /// 存活链的链头方向链接（指向上一个存活对象）；这是"存活链挂弱槽位"的落点。
    pub(crate) prev_live_: AtomicPtr<WeakChunk<()>>,
    /// 存活链的链尾方向链接。
    pub(crate) next_live_: AtomicPtr<WeakChunk<()>>,
    /// 本槽位对应的强块。数据的析构与访问都要经过它。
    pub(crate) strong_chunk_: AtomicPtr<StrongChunkBase>,
    /// 类型擦除析构登记。
    pub(crate) drop_: DropVtable,
    /// `T` 只在类型上区分，不参与布局。
    pub(crate) _unused_t_: PhantomData<NonNull<StrongChunk<T>>>,
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

/// 状态字的位布局：高 1 位锁、次 3 位 [`DataState`]、低 28 位**弱**引用计数。
///
/// 强引用计数**不在这里**，它由 [`StrongChunkBase`] 自己管理：两者数的是完全不同的东西。
#[repr(C)]
pub(crate) struct WeakChunkState {
    /// 在所属 `WeakPool` 中的索引（0 开始），也用于从槽位反推池地址。
    ///
    /// 只取决于槽位在池内数组中的位置，池在构造时一次写死，此后终生只读。
    pub(crate) pool_order_: PoolIndex,

    /// 协助 `WeakPool` 串联空闲槽位。
    pub(crate) next_freed_: PoolIndex,

    /// 状态字，布局见本结构文档。
    pub(crate) weak_state_: AtomicU32,
}

/// 把状态与计数打包成一个字。
const fn pack_(state: DataState, count: u32) -> u32 {
    ((state as u32) << WeakChunkState::DATA_SHIFT) | (count & WeakChunkState::WEAK_RC_MASK)
}

impl WeakChunkState {
    const DATA_ST_MASK: u32 = 0x7000_0000;
    const LOCK_ST_MASK: u32 = 0x8000_0000;
    const WEAK_RC_MASK: u32 = 0x0FFF_FFFF;
    const DATA_SHIFT: u32 = 28;

    /// 槽位尚未被分配给任何对象时的初值。
    pub(crate) const fn empty_() -> Self {
        WeakChunkState {
            pool_order_: 0,
            next_freed_: 0,
            weak_state_: AtomicU32::new(0),
        }
    }

    // -- 原子原语：所有"读—改—写"都经由 try_once_ ---------------------------

    /// 一次 `compare_exchange`。
    ///
    /// 这类操作在本文件出现多次（抢锁、释放、状态迁移、计数增减）。把 CAS 本身收口在
    /// 这里，调用方只负责计算 `desired`，避免同一种样板代码散落各处。
    fn try_once_(&self, current: u32, desired: u32) -> CmpxchResult<u32> {
        match self.weak_state_.compare_exchange(
            current,
            desired,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Result::Ok(_) => CmpxchResult::Succ(current),
            Result::Err(observed) => CmpxchResult::Fail(observed),
        }
    }

    /// 循环调用 [`WeakChunkState::try_once_`]，直到闭包判定"无需再试"。
    ///
    /// 闭包接收当前值、返回期望的新值；期望值与当前值相同即表示"不需要改动"，循环结束。
    /// 每次失败都用观察到的值重试，因此不会拿着过期状态空转。
    fn update_<F>(&self, mut desire: F)
    where
        F: FnMut(u32) -> u32,
    {
        let mut current = self.weak_state_.load(Ordering::Acquire);
        loop {
            let desired = desire(current);
            if desired == current {
                return;
            }
            match self.try_once_(current, desired) {
                CmpxchResult::Succ(_) => return,
                CmpxchResult::Fail(observed) | CmpxchResult::Unexpected(observed) => {
                    current = observed
                }
            }
        }
    }

    /// 抢占状态锁：循环调用一次性 CAS，直到成功或达到重试上限。
    ///
    /// **这是本文件里唯一一处抢锁循环**，有界重试与忙等只是它的两种参数：`max_try == 0`
    /// 表示不限次数。
    ///
    /// **私有**：锁的释放只允许发生在 [`WeakChunkStateGuard`] 的 `Drop` 里，因此不提供
    /// 任何"手动解锁"入口，也不允许外部单独抢锁——否则就会出现"锁被抢到、却没人负责
    /// 释放"的状态。
    fn acquire_(&self, max_try: usize) -> bool {
        let mut tried = 0usize;
        loop {
            let current = self.weak_state_.load(Ordering::Acquire);
            if (current & Self::LOCK_ST_MASK) == 0 {
                if self.try_once_(current, current | Self::LOCK_ST_MASK).is_succ() {
                    return true;
                }
            } else {
                core::hint::spin_loop();
            }
            tried += 1;
            if max_try != 0 && tried >= max_try {
                return false;
            }
        }
    }

    /// 释放状态锁。**只应由 [`WeakChunkStateGuard`] 的 `Drop` 调用。**
    fn release_lock_(&self) {
        self.update_(|current| current & !Self::LOCK_ST_MASK);
    }

    // -- 读写 ---------------------------------------------------------------

    /// 从状态字里取出 [`DataState`]。
    #[inline]
    fn state_of_(raw: u32) -> DataState {
        DataState::new(((raw & Self::DATA_ST_MASK) >> Self::DATA_SHIFT) as u8)
    }

    #[inline]
    pub(crate) fn data_state(&self) -> DataState {
        Self::state_of_(self.weak_state_.load(Ordering::Acquire))
    }

    /// 弱引用计数。
    #[inline]
    pub(crate) fn weak_count(&self) -> usize {
        (self.weak_state_.load(Ordering::Acquire) & Self::WEAK_RC_MASK) as usize
    }

    #[inline]
    pub(crate) fn incr_weak_count(&self) -> usize {
        let s = self.weak_state_.fetch_add(1, Ordering::AcqRel);
        ((s & Self::WEAK_RC_MASK) + 1) as usize
    }

    #[inline]
    pub(crate) fn decr_weak_count(&self) -> usize {
        let s = self.weak_state_.fetch_sub(1, Ordering::AcqRel);
        ((s & Self::WEAK_RC_MASK) - 1) as usize
    }

    /// 是否处于加锁状态。
    #[inline]
    pub(crate) fn is_locked(&self) -> bool {
        (self.weak_state_.load(Ordering::Acquire) & Self::LOCK_ST_MASK) != 0
    }

    // -- 状态迁移 -----------------------------------------------------------

    /// 比较并交换状态位，保持锁位与弱计数不变。
    ///
    /// 只有**真正由本次调用完成迁移**才返回 `Some`，"状态本来就已经是 `to`" 不算成功
    /// ——升级路径要靠这个返回值判断自己是不是赢家。
    pub(crate) fn compare_exchange_state(
        &self,
        from: DataState,
        to: DataState,
    ) -> Option<DataState> {
        loop {
            let current = self.weak_state_.load(Ordering::Acquire);
            if Self::state_of_(current) != from {
                return Option::None;
            }
            let desired = (current & !Self::DATA_ST_MASK) | ((to as u32) << Self::DATA_SHIFT);
            if self.try_once_(current, desired).is_succ() {
                return Option::Some(from);
            }
        }
    }

    /// 在持有【未加锁】的前提下按状态位做一次迁移。
    ///
    /// `desire` 返回 [`None`] 表示"不满足前提"，直接放弃；返回的目标状态与当前一致时
    /// 视为已经就位。迁移只改状态位，引用计数原样保留。
    fn try_update_state_<F>(&self, mut desire: F) -> Option<DataState>
    where
        F: FnMut(DataState) -> Option<DataState>,
    {
        loop {
            let current = self.weak_state_.load(Ordering::Acquire);
            let from = Self::state_of_(current);
            let to = desire(from)?;
            if to == from {
                return Option::Some(from);
            }
            let desired = (current & !Self::DATA_ST_MASK) | ((to as u32) << Self::DATA_SHIFT);
            if self.try_once_(current, desired).is_succ() {
                return Option::Some(from);
            }
        }
    }

    /// 认领销毁：把状态从 `{Created, Owning, Sharing}` 搬到 [`DataState::Destroying`]。
    ///
    /// 返回先前状态表示认领成功；返回 [`None`] 表示已有别的路径处理过（或该块从未携带
    /// 存活数据），调用方必须放弃析构。这是在"多路径析构"下保证**恰好销毁一次**的唯一
    /// 裁决点：`Owning::drop`、`Sharing::drop` 的最后一次减计数、以及清盘兜底，都只能
    /// 通过这里竞争。
    pub(crate) fn try_claim_destroy(&self) -> Option<DataState> {
        self.try_update_state_(|from| {
            from.is_claimable().then_some(DataState::Destroying)
        })
    }

    /// 标记数据已析构完成。只有成功认领过销毁的线程应当调用。
    pub(crate) fn mark_destroyed(&self) {
        let _ = self.try_update_state_(|from| (from == DataState::Destroying).then_some(DataState::Destroyed));
    }

    /// 标记已退出存活链、可随池回收。只有弱引用计数归零后才应调用。
    pub(crate) fn mark_finalized(&self) {
        let _ = self.try_update_state_(|_| Option::Some(DataState::Finalized));
    }

    /// 把状态置为 `Created`。分配路径在数据就位、槽位正式投入使用之后调用。
    pub(crate) fn init_created(&self) {
        let _ = self.try_update_state_(|_| Option::Some(DataState::Created));
    }

    /// 只在当前为 `Created` 时把状态置为 `to`；返回先前状态。
    ///
    /// 这个条件迁移同时充当"无强引用"这一前提的判据与上锁动作，因此不需要额外的锁位。
    pub(crate) fn try_set_state(&self, to: DataState) -> Option<DataState> {
        self.try_update_state_(|from| (from == DataState::Created).then_some(to))
    }

    /// 把状态无条件搬到 `to`（保持锁位与两个计数不变）。
    pub(crate) fn mark_state(&self, to: DataState) {
        self.update_(|current| {
            (current & !Self::DATA_ST_MASK) | ((to as u32) << Self::DATA_SHIFT)
        });
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
        self.weak_state_.store(0, Ordering::Release);
    }

    /// 首次初始化一个从未使用过的槽位的状态字：无条件写入初值。
    ///
    /// 与 [`WeakChunkState::reset_all`] 的区别是这里不做任何断言——此刻内存里是分配器
    /// 给的随机字节，没有"上一个使用者"可言。
    pub(crate) fn init_unallocated(&mut self) {
        self.weak_state_ = AtomicU32::new(0);
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

    /// 登记类型擦除析构所需的信息。
    ///
    /// `data` 是刚就地构造好的真实引用，因此这里能安全地取出 `?Sized` 的类型元数据，
    /// 让清盘流程之后能重建胖指针并把数据析构掉。
    pub fn set_drop_info(&mut self, data: *mut T) {
        let drop_fn_ = if mem::needs_drop::<T>() {
            Option::Some(drop_in_place_entry_::<T> as unsafe fn(*mut u8, *const ()))
        } else {
            Option::None
        };
        self.drop_ = DropVtable::new_(
            drop_fn_,
            data.cast::<u8>(),
            meta_to_raw_(ptr::metadata(data as *const T)),
        );
    }

    /// 登记类型擦除析构信息：调用方已经算好了数据区地址与类型元数据。
    ///
    /// 类型无关的调用方（例如 `StrongChunk`）走这个入口：在那里有具体的 `T`，可以算出
    /// 单态化的析构入口与元数据。
    pub(crate) fn set_drop_info_erased_(
        &mut self,
        drop_fn_: Option<unsafe fn(*mut u8, *const ())>,
        data_: *mut u8,
        meta_: *const (),
    ) {
        self.drop_ = DropVtable::new_(drop_fn_, data_, meta_);
    }

    /// 本槽位的析构登记。
    #[inline]
    pub(crate) fn drop_vtable(&self) -> DropVtable {
        self.drop_
    }

    /// 从登记信息就地析构数据。只有状态机认领成功后才允许调用。
    ///
    /// # Safety
    ///
    /// 必须先由状态机认领销毁，且恰好调用一次。
    pub(crate) unsafe fn drop_data(&self) {
        let Option::Some(drop_fn) = self.drop_.drop_fn_ else {
            return;
        };
        // SAFETY: 由调用方保证已认领且尚未析构；登记信息来自同一块数据
        unsafe { drop_fn(self.drop_.data_, self.drop_.meta_) };
    }

    // -- 数据访问（按 U 重建，支持 ?Sized）---------------------------------

    /// 按 `U` 重建数据引用。调用方必须保证数据仍然活着。
    ///
    /// 之所以能从"对 `T` 一无所知"的槽位上重建任意 `U`：数据区地址与 `?Sized` 的元数据
    /// 都由槽位的析构登记给出，两者合起来就是当初那块数据的胖指针。
    ///
    /// # Safety
    ///
    /// 数据必须尚未析构，且 `U` 必须与当初分配的 `T` 是同一个类型。
    pub(crate) unsafe fn data_ref_as<U: ?Sized + ptr::Pointee>(&self) -> &U {
        let data = self.drop_.data_;
        let meta = unsafe { raw_to_meta_::<U>(self.drop_.meta_) };
        // SAFETY: 由调用方保证数据尚未析构；元数据来自同一次分配的登记
        unsafe { &*ptr::from_raw_parts::<U>(data as *const (), meta) }
    }

    /// 按 `U` 重建数据可变引用。
    ///
    /// # Safety
    ///
    /// 同 [`WeakChunk::data_ref_as`]，且调用方必须保证独占访问。
    pub(crate) unsafe fn data_ref_mut_as<U: ?Sized + ptr::Pointee>(&mut self) -> &mut U {
        let data = self.drop_.data_;
        let meta = unsafe { raw_to_meta_::<U>(self.drop_.meta_) };
        // SAFETY: 由调用方保证数据尚未析构且独占
        unsafe { &mut *ptr::from_raw_parts_mut::<U>(data as *mut (), meta) }
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
            .compare_exchange_state(DataState::Created, DataState::Owning)
        {
            Option::Some(_) => Result::Ok(chunk),
            Option::None => Result::Err(self.data_state()),
        }
    }

    /// 尝试把 `Retain` 的持有关系升级为 `Sharing`，并把强引用计数置 1。
    ///
    /// # Errors
    ///
    /// 状态不是 `Created` 时返回当前状态。
    pub fn try_sharing(&self) -> Result<NonNull<StrongChunkBase>, DataState> {
        let Some(chunk) = self.strong_chunk() else {
            return Result::Err(self.data_state());
        };
        match self
            .chunk_state_
            .compare_exchange_state(DataState::Created, DataState::Sharing)
        {
            Option::Some(_) => {
                // SAFETY: 强块与身份槽位互相绑定，chunk 有效
                unsafe { &*chunk.as_ptr() }.incr_strong_count();
                Result::Ok(chunk)
            }
            Option::None => Result::Err(self.data_state()),
        }
    }

    // -- 池的维护 -----------------------------------------------------------

    /// 把槽位串成空闲链的一环，并清掉上一轮使用留下的状态与析构登记。
    ///
    /// 仅供 `WeakPool` 在归还槽位时调用。
    pub(crate) fn link_as_freed(&mut self, next: PoolIndex) {
        self.chunk_state_.set_next_freed(next);
        self.chunk_state_.reset_all();
        self.set_strong_chunk(ptr::null_mut());
        self.set_prev_live(ptr::null_mut());
        self.set_next_live(ptr::null_mut());
        self.drop_ = DropVtable::empty_();
    }

    /// 初始化一整段从未使用过的槽位，是 `WeakPool` 构造期唯一的槽位入口。
    ///
    /// - 按数组下标写死 `pool_order_`，此后终生只读；
    /// - 把空闲链接串成顺序链：`i` 指向 `i + 1`，末位指向 `capacity`，于是分配路径
    ///   无须区分「从未分配过的槽位」与「归还回来的槽位」；
    /// - 把状态、计数、存活链链接与析构登记一律拉到干净初值（分配器给的是未初始化内存）。
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
            slot.drop_ = DropVtable::empty_();
        }
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 持有弱槽位状态锁的守卫。
///
/// # 它与状态 CAS 的分工
///
/// `weak_state_` 里的状态迁移 CAS 只保证**"状态迁移唯一"**；而升级/初始化路径要保护的
/// 是一组彼此相关的写：弱引用计数、`strong_chunk_`、`drop_` 登记、以及强块的强计数。
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
unsafe fn raw_to_meta_<T: ?Sized + ptr::Pointee>(raw: *const ()) -> T::Metadata {
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
    weak.drop_.meta_
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

/// 取出 `T` 的单态化析构入口，供类型无关的持有者在登记析构信息时使用。
pub(crate) fn drop_entry_<T: ?Sized>() -> unsafe fn(*mut u8, *const ()) {
    drop_in_place_entry_::<T> as unsafe fn(*mut u8, *const ())
}

/// 单态化的析构入口：把类型擦除掉的地址与元数据还原成 `*mut T` 再 `drop_in_place`。
///
/// `?Sized` 的 `T` 需要重建胖指针，这正是把元数据一起存进弱槽位的原因。
///
/// # Safety
///
/// `data` 与 `meta` 必须来自同一块 `StrongChunk<T>` 的数据区，且该数据的 `Drop` 尚未执行。
unsafe fn drop_in_place_entry_<T: ?Sized>(data: *mut u8, meta: *const ()) {
    let meta = unsafe { raw_to_meta_::<T>(meta) };
    // SAFETY: 由调用方保证地址与元数据匹配
    let ptr = unsafe { ptr::from_raw_parts_mut::<T>(data as *mut (), meta) };
    // SAFETY: 由调用方保证该数据尚未析构
    unsafe { ptr::drop_in_place(ptr) };
}
