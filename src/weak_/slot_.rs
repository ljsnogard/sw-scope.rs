use core::{
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use abs_cancel::{NonCancellableToken, TrCancellationToken};
use atomex::{AtomicFlags, CmpxchResult};
use atomic_sync::{
    mutex::preemptive::MsbAsMutexSignal,
    x_deps::{abs_cancel, atomex},
};

use crate::{
    scope_inner_::PoolIndex,
    strong_::{StrongChunk, StrongChunkBase},
    weak_::WeakPool,
};

/// 弱引用槽位。它既是 `Retain<T>` 的持有对象，也是**对象的身份与生命周期上下文**。
///
/// `Retain<T>` 是类似 GC Handle 的指针：句柄传递到哪里，生命周期便带到哪里。而
/// `Owning<'a, T>` / `Sharing<'a, T>` 都是从 `Retain` 上"借"出来的伴生视图，它们的 `'a`
/// 绑定在 `&Retain` 上，**不可能比产生它们的那个 `Retain` 活得更久**。因此：
///
/// > 数据还活着 ⇒ 至少有一个 `Retain` 活着 ⇒ 这个 `WeakSlot` 还活着。
///
/// 这条不等式是后续所有设计的地基：弱槽位在强块数据活着的整个期间都不会被回收复用，
/// 因此它可以安全地承担对象身份、状态与空闲链，而不需要强块反过来保存逐对象的身份。
///
/// # 新设计中的职责划分
///
/// - 弱槽位只保存状态字（[`WeakSlotState`]）与指向强块的指针；
/// - `PreDrop` 登记与 `?Sized` 元数据保存在强块头部（`StrongChunkBase`）；
/// - **存活链不再逐槽位保存 `prev` / `next`**：它由池链、槽位下标与「空闲/已分配」
///   状态在现场反推，见 [`WeakSlotState::for_each_live_slot_`]。
///
/// # 何时可以被回收
///
/// **不是** `weak_count` 归零就算数。槽位的回收要综合 [`WeakSlotState::weak_state_`]
/// 里的状态与两个计数判断：当"没有任何 `Retain`"且"数据已经析构（或从未需要析构）"时，
/// 槽位才归还给 [`crate::weak_::WeakPool`]。
///
/// # 布局
///
/// 弱槽位与强块分属两个池，这里是弱池里的数组元素。槽位尺寸与 `T` 无关：`T` 只出现在
/// [`PhantomData`] 中，因此池可以统一按 `WeakSlot<()>` 的步长定位槽位。
#[repr(C)]
pub(crate) struct WeakSlot<T>
where
    T: ?Sized,
{
    /// 状态、弱计数与空闲链链接。
    ///
    /// **必须是第一个字段**：`WeakSlotState::acquire_` 只收到 `&self`，要靠这个布局
    /// 不变量把状态地址直接当作 `WeakSlot` 地址使用。见本文件末尾的编译期断言。
    slot_state_: WeakSlotState,
    /// 本槽位对应的强块。数据的析构、类型擦除信息与 `PreDrop` 都在强块头部，
    /// 弱槽位只负责指出它。
    strong_chunk_: AtomicPtr<StrongChunkBase>,
    /// `T` 只在类型上区分，不参与布局。
    _unused_t_: PhantomData<NonNull<StrongChunk<T>>>,
}

// `WeakSlotState` 是 `WeakSlot` 的首字段：`#[repr(C)]` 保证其偏移为 0。
// 这条不变量是 `WeakSlotState::acquire_` 只凭自身地址反推 `WeakSlot` 的前提。
const _: () = assert!(core::mem::offset_of!(WeakSlot<()>, slot_state_) == 0);

/// `WeakSlot` 的生命周期状态。**这是"数据要不要析构"的唯一权威来源**，
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
    /// 这是"已纳入 GC 队列"的终态：存活链由池链与槽位状态反推，状态位负责记录
    /// "这块数据销毁到哪一步"。
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
type StateWord = AtomicFlags<u32, AtomicU32>;

/// 状态字的位布局：高 1 位锁、次 3 位 [`DataState`]、低 28 位**弱**引用计数。
///
/// 强引用计数**不在这里**，它由 [`StrongChunkBase`] 自己管理：两者数的是完全不同的东西。
#[repr(C)]
pub(crate) struct WeakSlotState {
    /// 在所属 `WeakPool` 中的索引（0 开始），也用于从槽位反推池地址。
    ///
    /// 只取决于槽位在池内数组中的位置，池在构造时一次写死，此后终生只读。
    pool_order_: PoolIndex,

    /// 协助 `WeakPool` 串联空闲槽位。
    next_freed_: PoolIndex,

    /// 状态字；锁位、状态位与弱计数的读写全部经由 [`AtomicFlags`] 的操作。
    weak_state_: StateWord,
}

/// 把状态与计数打包成一个字。
const fn pack_(state: DataState, count: u32) -> u32 {
    ((state as u32) << WeakSlotState::DATA_SHIFT) | (count & WeakSlotState::WEAK_RC_MASK)
}

impl WeakSlotState {
    /// 最高位用于表示锁状态
    const LOCK_ST_MASK: u32 = 0x8000_0000;

    /// 最高位后3位用于表示 DataState
    const DATA_ST_MASK: u32 = 0x7000_0000;

    /// 低 28 位表示弱引用计数
    const WEAK_RC_MASK: u32 = 0x0FFF_FFFF;
    const DATA_SHIFT: u32 = 28;

    /// 槽位尚未被分配给任何对象时的初值。
    ///
    /// 因为 [`AtomicFlags::new`] 要构造原子字，所以本函数不是 `const fn`。
    pub(crate) fn empty_() -> Self {
        WeakSlotState {
            pool_order_: 0,
            next_freed_: 0,
            weak_state_: AtomicFlags::new(AtomicU32::new(0)),
        }
    }

    // -- 锁：以状态字最高位为锁信号 -------------------------------------------

    /// 抢占状态锁：以最高位为锁信号。
    ///
    /// `cancel` 是**自旋过程中放弃抢锁**的信号：本方法至少尝试一次 CAS；若没抢到，
    /// 每轮自旋前检查令牌，已取消就返回 [`None`]。因此：
    ///
    /// - 传 [`NonCancellableToken`] 表示一直忙等；
    /// - 传 [`CancelledToken`](abs_sync::x_deps::abs_cancel::CancelledToken) 表示
    ///   "只试一次"的非阻塞语义（第一次没抢到就放弃）。
    ///
    /// **私有**：锁的释放只允许发生在 [`WeakSlotGuard`] 的 `Drop` 里，因此不提供
    /// 任何"手动解锁"入口，也不允许外部单独抢锁——否则就会出现"锁被抢到、却没人负责
    /// 释放"的状态。
    ///
    /// 调用方不需要再传 `&WeakSlot`：`WeakSlotState` 是 `WeakSlot` 的首字段，状态地址
    /// 就是槽位地址。
    fn acquire_<T, K>(&self, cancel: K) -> Option<WeakSlotGuard<'_, T>>
    where
        T: ?Sized,
        K: TrCancellationToken,
    {
        // `WeakSlotState` 是 `WeakSlot` 的第一个字段（`#[repr(C)]` + 编译期断言），
        // 因此状态自身的地址就是槽位的地址，不需要调用方再传 `&WeakSlot`。
        //
        // SAFETY: 上面的布局不变量保证该转换不会移动地址；`T` 只影响
        // `WeakSlot<T>` 中 `PhantomData` 的类型，不影响布局。
        let owner = unsafe { NonNull::new_unchecked(self as *const Self as *mut WeakSlot<T>) };

        // 这里直接基于状态字的最高位做 CAS：只有锁位为 0 时才能置位。
        //
        // 每一轮都重新读取状态字：竞争者持锁时 CAS 根本不会执行，若继续使用旧的
        // `current` 就会永远看不到对方释放锁，形成忙等死锁。
        loop {
            let current = self.weak_state_.value();
            if !Self::is_locked_raw_(current) {
                match self.weak_state_.try_once_compare_exchange_weak(
                    current,
                    |raw| !Self::is_locked_raw_(raw),
                    |raw| raw | Self::LOCK_ST_MASK,
                ) {
                    CmpxchResult::Succ(_) => {
                        return Option::Some(WeakSlotGuard {
                            owner_: owner,
                            state_: NonNull::from(self),
                            _unuse_t_: PhantomData,
                        });
                    }
                    // 竞争失败：下一轮重新读状态字即可。
                    CmpxchResult::Fail(_) | CmpxchResult::Unexpected(_) => {}
                }
            }
            // 至少尝试过一次之后，才把取消信号作为放弃抢锁的依据。
            if cancel.is_cancelled() {
                return Option::None;
            }
            core::hint::spin_loop();
        }
    }

    /// 状态字里的锁位是否已置位。
    #[inline]
    fn is_locked_raw_(raw: u32) -> bool {
        raw & Self::LOCK_ST_MASK != 0
    }

    /// 释放状态锁。**只应由 [`WeakSlotGuard`] 的 `Drop` 调用。**
    fn release_lock_(&self) {
        let mut current = self.weak_state_.value();
        loop {
            if !Self::is_locked_raw_(current) {
                return;
            }
            match self.weak_state_.try_once_compare_exchange_weak(
                current,
                Self::is_locked_raw_,
                |raw| raw & !Self::LOCK_ST_MASK,
            ) {
                CmpxchResult::Succ(_) => return,
                CmpxchResult::Fail(actual) | CmpxchResult::Unexpected(actual) => {
                    current = actual;
                }
            }
        }
    }

    // -- 读写 ---------------------------------------------------------------

    /// 从状态字里取出 [`DataState`]。
    #[inline]
    fn state_of_(raw: u32) -> DataState {
        DataState::new(((raw & Self::DATA_ST_MASK) >> Self::DATA_SHIFT) as u8)
    }

    #[inline]
    pub(crate) fn data_state(&self) -> DataState {
        Self::state_of_(self.weak_state_.value())
    }

    /// 弱引用计数。
    #[inline]
    pub(crate) fn weak_count(&self) -> usize {
        (self.weak_state_.value() & Self::WEAK_RC_MASK) as usize
    }

    #[inline]
    pub(crate) fn incr_weak_count(&self) -> usize {
        self.update_weak_count_(1)
    }

    #[inline]
    pub(crate) fn decr_weak_count(&self) -> usize {
        self.update_weak_count_(-1)
    }

    /// 在保持锁位与状态位不变的前提下给弱计数加上 `delta`，返回新计数。
    ///
    /// 用 CAS 重试而不是 `fetch_add`，是因为弱计数、状态与锁位共用一个字；只有把
    /// "读出旧值 → 只改计数位"作为一个整体提交，才不会覆盖并发的状态迁移。
    fn update_weak_count_(&self, delta: i32) -> usize {
        let mut current = self.weak_state_.value();
        loop {
            let old_count = current & Self::WEAK_RC_MASK;
            let new_count = if delta >= 0 {
                old_count.wrapping_add(delta as u32)
            } else {
                old_count.wrapping_sub((-delta) as u32)
            } & Self::WEAK_RC_MASK;
            let next = (current & !Self::WEAK_RC_MASK) | new_count;
            match self
                .weak_state_
                .try_once_compare_exchange_weak(current, |_| true, |_| next)
            {
                CmpxchResult::Succ(_) => return new_count as usize,
                CmpxchResult::Fail(actual) | CmpxchResult::Unexpected(actual) => {
                    current = actual;
                }
            }
        }
    }

    /// 是否处于加锁状态。
    #[inline]
    pub(crate) fn is_locked(&self) -> bool {
        self.weak_state_.value() & Self::LOCK_ST_MASK != 0
    }

    /// 仅凭本状态反推整条存活链，并按"相对构造顺序"逐个访问存活槽位。
    ///
    /// # 为什么只凭 `WeakSlotState` 就够了
    ///
    /// 1. `WeakSlotState` 是 `WeakSlot` 的首字段，所以状态地址就是槽位地址；
    /// 2. 槽位里的 `pool_order_` 是它在弱池中的下标，且池头大小固定，因此由槽位地址
    ///    减去 `HEADER_BYTES + pool_order * SLOT_SIZE` 就得到所属 [`WeakPool`]；
    /// 3. 每个池都带 `prev_` / `next_` 兄弟链，从任意池都能走到链尾；
    /// 4. 链尾池的槽位状态足以区分"已分配"与"空闲"，于是可以从链尾池的最后一个空闲
    ///    槽位开始，按全局下标顺序回绕遍历所有池，跳过空闲槽位。
    ///
    /// 这套推导成立的关键是弱池的分配顺序：新槽位按全局下标递增分配，归还的槽位会被
    /// 优先复用。因此"全局下标顺序去掉空闲空洞"就是一条相对有序的构造顺序；被复用的
    /// 槽位会回到原下标位置，所以是"相对"顺序，而不是精确的分配时间顺序。
    ///
    /// 详细算法见 [`WeakPool::for_each_live_from_`]。
    ///
    /// # 归属范围
    ///
    /// 遍历范围是"本状态所在池的整条兄弟链"。若一条池链只属于一个 `Scope`，结果就是
    /// 该 `Scope` 的全部存活槽位；若多个 `Scope` 共享同一条池链，上层还需要按归属
    /// 进一步过滤。
    pub(crate) fn for_each_live_slot_(&self, mut visit: impl FnMut(NonNull<WeakSlot<()>>)) {
        // SAFETY: `WeakSlotState` 是 `WeakSlot` 的首字段（见本文件的编译期断言）
        let slot = unsafe { NonNull::new_unchecked(self as *const Self as *mut WeakSlot<()>) };
        // 槽位一定落在某个弱池里；找不到池说明调用方给了非池内槽位，直接不遍历。
        if let Option::Some(pool) = WeakPool::of_slot_(slot) {
            WeakPool::for_each_live_from_(pool, &mut visit);
        }
    }

    // -- 状态迁移：CAS 隐藏在 `transition_` 里 -------------------------------

    /// 把字里的状态位从 `from` 换成 `to`，保持锁位与弱计数不变。
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
        // 旧实现直接使用 `SpinFlag::try_update`（已随 `atomic_` 模块删除）。这里用
        // `AtomicFlags` 的 CAS 原语重写：`desire` 返回 `None` 表示前提不成立，直接放弃。
        let mut current = self.weak_state_.value();
        loop {
            let from = Self::state_of_(current);
            let to = desire(from)?;
            let next = if to == from {
                current
            } else {
                Self::with_state_(current, to)
            };
            match self
                .weak_state_
                .try_once_compare_exchange_weak(current, |_| true, |_| next)
            {
                CmpxchResult::Succ(_) => return Option::Some(from),
                CmpxchResult::Fail(actual) | CmpxchResult::Unexpected(actual) => {
                    current = actual;
                }
            }
        }
    }

    // 旧实现（依赖已删除的 `SpinFlag::try_update`）：
    //
    // fn transition_<F>(&self, mut desire: F) -> Option<DataState>
    // where
    //     F: FnMut(DataState) -> Option<DataState>,
    // {
    //     self.weak_state_.try_update(|raw| {
    //         let from = Self::state_of_(raw);
    //         let to = desire(from)?;
    //         if to == from {
    //             return Option::Some((raw, from));
    //         }
    //         Option::Some((Self::with_state_(raw, to), from))
    //     })
    // }

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
        let _ = self.transition_(|_| Option::Some(to));
        // 旧实现：
        //
        // let _ = self
        //     .weak_state_
        //     .try_update(|raw| Option::Some((Self::with_state_(raw, to), ())));
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
    /// 必须走 [`WeakSlotState::init_unallocated`]，否则会对随机内容做断言。
    pub(crate) fn reset_all(&mut self) {
        // 归还时还锁着，说明有线程卡在升级/初始化路径里，这属于逻辑错误
        debug_assert!(!self.is_locked(), "归还槽位时状态锁应当已经释放");
        self.weak_state_ = AtomicFlags::new(AtomicU32::new(0));
    }

    /// 首次初始化一个从未使用过的槽位的状态字：无条件写入初值。
    ///
    /// 与 [`WeakSlotState::reset_all`] 的区别是这里不做任何断言——此刻内存里是分配器
    /// 给的随机字节，没有"上一个使用者"可言。
    pub(crate) fn init_unallocated(&mut self) {
        self.weak_state_ = AtomicFlags::new(AtomicU32::new(0));
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> WeakSlot<T>
where
    T: ?Sized,
{
    #[inline]
    pub fn data_state(&self) -> DataState {
        self.slot_state_.data_state()
    }

    /// 槽位当前是否空闲。
    ///
    /// "空闲"等价于状态为 [`DataState::Reclaimed`]：从未分配过的槽位与归还回池的
    /// 槽位都是这个状态。存活链推导据此把空闲槽位当作顺序上的空洞跳过。
    #[inline]
    pub(crate) fn is_free_(&self) -> bool {
        self.data_state() == DataState::Reclaimed
    }

    /// 读取槽位当前的弱引用计数。
    #[inline]
    pub fn weak_count(&self) -> usize {
        self.slot_state_.weak_count()
    }

    #[inline]
    pub fn incr_weak_count(&self) -> usize {
        self.slot_state_.incr_weak_count()
    }

    #[inline]
    pub fn decr_weak_count(&self) -> usize {
        self.slot_state_.decr_weak_count()
    }

    /// 直接抬高弱引用计数，仅供测试构造"还有 `Retain` 在世"的场景。
    #[cfg(test)]
    pub fn incr_use_count(&self) -> usize {
        self.slot_state_.incr_weak_count()
    }

    #[inline]
    pub fn pool_order(&self) -> PoolIndex {
        self.slot_state_.pool_order()
    }

    #[inline]
    pub fn next_freed(&self) -> PoolIndex {
        self.slot_state_.next_freed()
    }

    // -- 状态锁 -------------------------------------------------------------

    /// 尝试获取本槽位的状态锁；`cancel` 是自旋过程中放弃抢锁的信号。
    ///
    /// 至少尝试一次 CAS：成功立即返回守卫；没抢到且令牌已取消时返回 [`None`]。
    /// 因此传 [`NonCancellableToken`] 是忙等，传
    /// [`CancelledToken`](abs_sync::x_deps::abs_cancel::CancelledToken) 是"只试一次"。
    /// 锁的释放只由守卫的析构负责。
    pub fn try_lock<K>(&self, cancel: K) -> Option<WeakSlotGuard<'_, T>>
    where
        K: TrCancellationToken,
    {
        WeakSlotGuard::try_acquire_(self, cancel)
    }

    /// [`WeakSlot::try_lock`] 的别名，与状态层上的命名保持对应。
    #[inline]
    pub fn try_acq_state_guard<K>(&self, cancel: K) -> Option<WeakSlotGuard<'_, T>>
    where
        K: TrCancellationToken,
    {
        self.try_lock(cancel)
    }

    /// 忙等直到拿到状态锁。
    ///
    /// 供确信锁会很快释放的阻塞式路径使用；锁总会在守卫析构时释放，因此不会永久自旋。
    pub fn lock_busy(&self) -> WeakSlotGuard<'_, T> {
        WeakSlotGuard::spin_acquire_(self)
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
            .slot_state_
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
            .slot_state_
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
    /// - 把状态、计数与强块指针一律拉到干净初值（分配器给的是未初始化内存）。
    ///
    /// 存活链不再保存在槽位上：它由池链与空闲链在需要时反推，见
    /// [`WeakSlotState::for_each_live_slot_`]。`PreDrop` 与 `?Sized` 元数据也已移到
    /// `StrongChunkBase`，槽位只保留状态与强块指针。
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
            slot.slot_state_.pool_order_ = i as PoolIndex;
            slot.slot_state_.set_next_freed(next);
            slot.slot_state_.init_unallocated();
            slot.set_strong_chunk(ptr::null_mut());
        }
    }

    /// 槽位状态（锁、弱计数与空闲链链接）。
    #[inline]
    pub(crate) fn chunk_state(&self) -> &WeakSlotState {
        &self.slot_state_
    }

    /// 槽位状态的可变引用，供弱池在分配 / 归还时改写空闲链。
    #[inline]
    pub(crate) fn chunk_state_mut(&mut self) -> &mut WeakSlotState {
        &mut self.slot_state_
    }

    /// 造一个栈上的空槽位，仅供单元测试使用。
    #[cfg(test)]
    pub(crate) fn stack_empty_() -> Self {
        WeakSlot {
            slot_state_: WeakSlotState::empty_(),
            strong_chunk_: AtomicPtr::new(ptr::null_mut()),
            _unused_t_: PhantomData,
        }
    }

    // -- 池的维护 -----------------------------------------------------------

    /// 把槽位串成空闲链的一环，并清掉上一轮使用留下的状态与强块指针。
    ///
    /// `PreDrop` 登记与 `?Sized` 元数据属于强块，归还槽位时不需要在这里清。
    ///
    /// 仅供 `WeakPool` 在归还槽位时调用。
    pub(crate) fn link_as_freed(&mut self, next: PoolIndex) {
        self.slot_state_.set_next_freed(next);
        self.slot_state_.reset_all();
        self.set_strong_chunk(ptr::null_mut());
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 持有弱槽位状态锁的守卫。
///
/// # 它与状态 CAS 的分工
///
/// `weak_state_` 里的状态迁移 CAS 只保证**"状态迁移唯一"**；而升级/初始化路径要保护的
/// 是一组彼此相关的写：弱引用计数、`strong_chunk_`、以及强块的强计数。
/// 这些字段不在同一个字里，CAS 管不住它们的组合，于是需要本守卫把"读判据 + 一组写"
/// 合成一个临界区：
///
/// - `try_owning` / `try_sharing`：锁住之后才检查状态、绑定强块、抬计数；
/// - 分配路径：锁住之后才把"已就位的数据"正式置为 `Created`，避免外界看到半成品。
///
/// 本守卫 `Deref` 到 [`WeakSlot`]，因此可以在临界区内改动槽位字段。`Drop` 负责释放
/// 锁位——即使临界区里 panic，锁也一定被归还。
pub(crate) struct WeakSlotGuard<'a, T>
where
    T: ?Sized,
{
    /// 本守卫持有的槽位。
    owner_: NonNull<WeakSlot<T>>,
    /// 槽位状态字所在位置；守卫析构时据此解锁。
    state_: NonNull<WeakSlotState>,
    _unuse_t_: PhantomData<&'a WeakSlot<T>>,
}

// 旧字段定义：
//
// slot_ptr_: NonNull<WeakSlot<T>>,
// _unuse_t_: PhantomData<&'a WeakSlot<T>>,

impl<'a, T> WeakSlotGuard<'a, T>
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
    fn try_acquire_<K>(
        owner: &'a WeakSlot<T>,
        cancel: K,
    ) -> Option<Self>
    where
        K: TrCancellationToken,
    {
        owner.slot_state_.acquire_::<T, K>(cancel)
    }

    /// 使用不可取消的令牌忙等抢锁，因此必然成功。
    ///
    /// 与 [`WeakSlotGuard::try_acquire_`] 共用同一条抢锁路径，只是令牌不同。
    fn spin_acquire_(owner: &'a WeakSlot<T>) -> Self {
        // `NonCancellableToken` 永远不会取消，因此这里可以解包
        Self::try_acquire_(owner, NonCancellableToken::new())
            .expect("不可取消的抢锁必定成功")
    }

    /// 本守卫持有的槽位。
    #[inline]
    pub(crate) fn chunk(&self) -> &'a WeakSlot<T> {
        // SAFETY: owner_ 来自调用方的借用，守卫的生命周期不超过它
        unsafe { self.owner_.as_ref() }
    }

    /// 在临界区内取可变引用。锁保证同一时刻只有一个守卫，因此独占成立。
    #[inline]
    pub(crate) fn chunk_mut(&mut self) -> &'a mut WeakSlot<T> {
        // SAFETY: 持锁即独占；守卫的 Drop 会释放锁
        unsafe { &mut *self.owner_.as_ptr() }
    }
}

impl<'a, T> core::ops::Deref for WeakSlotGuard<'a, T>
where
    T: ?Sized,
{
    type Target = WeakSlot<T>;

    fn deref(&self) -> &Self::Target {
        self.chunk()
    }
}

impl<'a, T> core::ops::DerefMut for WeakSlotGuard<'a, T>
where
    T: ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.chunk_mut()
    }
}

impl<'a, T> Drop for WeakSlotGuard<'a, T>
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
/// 偏移交给编译器在这个单态化里算，所以强块头部只需保存 `?Sized` 元数据，不必保存
/// 数据地址本身。
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
