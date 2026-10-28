use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::{self, NonNull},
    sync::atomic::{AtomicUsize, Ordering},
};

use atomex::LocksOrderings;
use atomic_sync::{
    mutex::preemptive::SpinningMutexOwned, rwlock::cooperative_arena::CooperativeRwLock,
};

use crate::{
    scope_inner_::RootScope,
    strong_::{StrongChunk, StrongChunkBase},
    weak_::{WeakPool, WeakSlot},
    x_deps::{atomex, atomic_sync},
};

/// 弱池链：链头 / 链尾 + 它们共同的自旋锁。
///
/// 弱池的“分配新池”（域写锁内）、“归还末槽后摘链”（任意 `Retain` drop 线程）与
/// “flush 期间钉住整条链”都要改这条链，因此必须有一把独立的链锁，而不能复用域写锁：
/// `flush_data_` 会在持有域写锁时级联析构数据，而数据里的 `Retain` drop 可能再次进入
/// 弱池归还路径；若归还路径也要域写锁，就会在同一个线程上自死锁。
///
/// 锁顺序固定为 **域写锁 → 弱池链锁 → 池锁**。链锁临界区内只做指针操作与非常短的
/// 池级操作，不调用用户代码。
pub(crate) struct WeakChain {
    /// 链头池；空链为 `None`。
    pub(super) head_: Option<NonNull<WeakPool>>,
    /// 链尾池；空链为 `None`。
    pub(super) tail_: Option<NonNull<WeakPool>>,
}

impl WeakChain {
    /// 空链。
    pub(super) const fn empty_() -> Self {
        WeakChain {
            head_: Option::None,
            tail_: Option::None,
        }
    }

    /// 把 `pool` 接到链尾，并回填它的所属域。
    pub(super) fn push_back_(&mut self, pool: NonNull<WeakPool>, scope: NonNull<ScopeInner>) {
        // SAFETY: pool 是刚初始化、尚未共享出去的独占池；调用方持有链锁
        let pool_ref = unsafe { &mut *pool.as_ptr() };
        pool_ref.set_scope(scope);
        pool_ref.set_prev_(self.tail_);
        pool_ref.set_next_(Option::None);
        match self.tail_ {
            // SAFETY: 链上的池仍然存活（摘链与释放都要先持有本锁）
            Option::Some(mut tail) => unsafe { tail.as_mut() }.set_next_(Option::Some(pool)),
            Option::None => self.head_ = Option::Some(pool),
        }
        self.tail_ = Option::Some(pool);
    }

    /// 把 `pool` 从链上摘下；调用方必须持有链锁并已把它标记为 `detached_`。
    pub(super) fn unlink_(&mut self, pool: NonNull<WeakPool>) {
        // SAFETY: 调用方保证 pool 仍在链上
        let (prev, next) = {
            let pool_ref = unsafe { pool.as_ref() };
            (pool_ref.prev(), pool_ref.next())
        };
        match prev {
            // SAFETY: prev 是链上的邻居，仍然存活
            Option::Some(mut prev) => unsafe { prev.as_mut() }.set_next_(next),
            Option::None => self.head_ = next,
        }
        match next {
            // SAFETY: next 是链上的邻居，仍然存活
            Option::Some(mut next) => unsafe { next.as_mut() }.set_prev_(prev),
            Option::None => self.tail_ = prev,
        }
    }

    /// 取出链头，用于 unpin 之后在锁外释放。
    pub(super) fn take_all_(&mut self) -> Option<NonNull<WeakPool>> {
        let head = self.head_.take();
        self.tail_ = Option::None;
        head
    }
}

/// 一个域的内部表示。它总是内联在实体自己的 [`ScopeLock`] 里。
///
/// 新设计不再有单独的 `ScopeHeader`：`Scope` 直接指向域实体的
/// `StrongChunk<ScopeEntity<A>>`，从 chunk 的数据地址就能拿到锁与分配器。句柄计数、
/// `closed` / `close_requested` 都是原子字段，可以在加锁前读写；`lock_` 指回包含自身的
/// 锁，供 `Retain` / `Owning` / `Sharing` 的收尾路径重试延迟清盘。
///
/// # root 回指
///
/// 多 root 之后，类型表是 per-root 的，因此弱槽位必须能沿
/// `WeakSlot -> WeakPool.scope_ -> ScopeInner.root_ -> RootScope.metas_` 找回自己的 root。
/// `root_` 为 `None` 只出现在不挂 root 的低层单元测试与 root 自己的实体池构造过程中。
///
/// # 实体自身的登记
///
/// 实体不需要额外登记：root 实体池本身就是一个 [`ScopeInner`]，实体的身份槽位就在它的
/// 弱池里。root 最终清理时直接用与普通域相同的弱池遍历算法（`flush_data_`）枚举实体、
/// 执行实体 vtable 析构，因此本结构不再保存 `entity_next_` / `entity_slot_`。
///
/// 域持有若干 [`crate::strong_::StrongPool`] 组成的链，池内的对象位置终身不变；域静默时
/// 整条链一次性回收。弱槽位池（[`WeakPool`]）同样按域持有，因此每个域的存活槽位只在本域
/// 的池链上。
/// `handles_` 高 3 位的状态标志；其余低位是 `Scope` 句柄计数。
///
/// 合并成一个原子字之后，“最后一个句柄析构”可以在同一次 CAS 里同时完成计数减一与
/// `CLOSED` 置位，不再有“计数已归零、closed 仍为 false”的多字窗口。
const FLAG_CLOSED: usize = 1 << (usize::BITS - 1);
const FLAG_CLOSE_REQUESTED: usize = 1 << (usize::BITS - 2);
const FLAG_RETIRED: usize = 1 << (usize::BITS - 3);
const FLAG_MASK: usize = FLAG_CLOSED | FLAG_CLOSE_REQUESTED | FLAG_RETIRED;
/// 低位句柄计数掩码。
const HANDLE_COUNT_MASK: usize = !FLAG_MASK;

#[repr(C)]
pub(crate) struct ScopeInner {
    /// `Scope` 句柄计数 + 三个状态位，打包在同一个原子字里。
    ///
    /// - 最高位：最后一个句柄已析构，或已显式 close；
    /// - 次高位：显式请求关闭（只有它为真时才执行延迟清盘）；
    /// - 第三高位：已完成“句柄归零 + 数据清空 + 弱池清空”的退出登记；
    /// - 其余低位：`Scope` 句柄计数。
    ///
    /// `Scope::clone` / `Drop` 只对这一个字做原子操作。
    pub(super) handles_: AtomicUsize,

    /// 弱池链；分配新池、归还末槽摘链、flush 钉住都要先拿它的自旋锁。
    pub(super) weak_chain_: SpinningMutexOwned<WeakChain>,

    /// 指向 Scope 所持有的第一块强池。
    pub(super) strong_head_: Option<NonNull<crate::strong_::StrongPool>>,

    /// 指向 Scope 所持有的最新一块强池。
    pub(super) strong_tail_: Option<NonNull<crate::strong_::StrongPool>>,

    /// 指向本实体尾部的分配器值。
    ///
    /// 每域一份分配器，因此这里不能是 `&'static`；指针在实体落位后指向同一实体里的
    /// `alloc_` 字段，实体的字段顺序保证锁先于分配器析构。
    pub(super) allocator_: NonNull<dyn Allocator + Send + Sync>,

    /// 开新弱池 / 强池时使用的页大小；由创建期的 `C::PageSize` 决定。
    pub(super) page_size_: usize,

    /// 指回包含本结构的 `ScopeLock`；root 自己的实体池没有锁，因此是 `None`。
    ///
    /// 只用于从弱槽位的 `scope_` 指针回到锁，以便 `Owning` / `Sharing` 释放后重试
    /// 延迟清盘。它不是 root 指针，也不参与池布局。
    pub(super) lock_: Option<NonNull<ScopeLock>>,

    /// 本域所属的 root；类型表查找与最终清理都从这里出发。
    pub(super) root_: Option<NonNull<RootScope>>,

    /// 当前有多少线程正持有指向本实体的“使用租约”。
    ///
    /// `Scope::drop` / `Retain::drop` 在接触实体之前先加一，退出时减一。退出登记要求
    /// `users_ == 1`（只有当前线程），因此不会在另一个线程还拿着实体指针时把实体从
    /// root 实体池里释放掉。
    pub(super) users_: AtomicUsize,
}

// SAFETY: 池链只在持有实体写锁 / 弱池链锁时访问；弱槽位对 `Retain` 的并发访问走自身的
// 原子状态；分配器要求 `Send + Sync`。实体一旦发布就不再移动。
unsafe impl Send for ScopeInner {}
// SAFETY: 见上。
unsafe impl Sync for ScopeInner {}

impl ScopeInner {
    /// 造一个没有池、只绑定了分配器的空域。
    pub(super) fn empty_(
        allocator: NonNull<dyn Allocator + Send + Sync>,
        page_size: usize,
    ) -> Self {
        ScopeInner {
            handles_: AtomicUsize::new(0),
            weak_chain_: SpinningMutexOwned::new_owned(WeakChain::empty_()),
            strong_head_: Option::None,
            strong_tail_: Option::None,
            allocator_: allocator,
            page_size_: page_size,
            lock_: Option::None,
            root_: Option::None,
            users_: AtomicUsize::new(0),
        }
    }

    /// 绑定本域所属的 root。
    pub(super) fn set_root_(&mut self, root: NonNull<RootScope>) {
        self.root_ = Option::Some(root);
    }

    /// 本域所属的 root。
    #[inline]
    pub(crate) const fn root_(&self) -> Option<NonNull<RootScope>> {
        self.root_
    }

    /// 在弱池链锁内访问链。
    ///
    /// # Panics
    ///
    /// 自旋锁内部出错（理论上不会发生）时 panic；与 crate 其它位置的自旋锁用法一致。
    pub(crate) fn with_weak_chain_<R>(&self, op: impl FnOnce(&mut WeakChain) -> R) -> R {
        let mut session = self.weak_chain_.lock_session();
        let mut guard = match session.lock().wait() {
            Result::Ok(guard) => guard,
            Result::Err(_) => unreachable!("weak chain 自旋锁不应失败"),
        };
        op(&mut guard)
    }

    /// 当前弱池链的链头。
    pub(crate) fn weak_head_(&self) -> Option<NonNull<WeakPool>> {
        self.with_weak_chain_(|chain| chain.head_)
    }

    /// 当前弱池链的链尾。
    pub(crate) fn weak_tail_(&self) -> Option<NonNull<WeakPool>> {
        self.with_weak_chain_(|chain| chain.tail_)
    }

    /// 当前句柄计数（不含高 3 位状态）。
    #[inline]
    pub(crate) fn handle_count_(&self) -> usize {
        self.handles_.load(Ordering::Acquire) & HANDLE_COUNT_MASK
    }

    /// 最后一个 `Scope` 句柄是否已经析构或显式关闭。
    #[inline]
    pub(crate) fn is_closed_(&self) -> bool {
        self.handles_.load(Ordering::Acquire) & FLAG_CLOSED != 0
    }

    /// 是否已经显式请求关闭。
    #[inline]
    pub(crate) fn is_close_requested_(&self) -> bool {
        self.handles_.load(Ordering::Acquire) & FLAG_CLOSE_REQUESTED != 0
    }

    /// 是否已经完成退出登记。
    #[inline]
    pub(crate) fn is_retired_(&self) -> bool {
        self.handles_.load(Ordering::Acquire) & FLAG_RETIRED != 0
    }

    /// 请求关闭：拒绝新放入，并立即尝试清盘。
    ///
    /// 返回值与 [`ScopeInner::try_flush_`] 相同；调用方若拿到 `Some(root)`，必须在完全
    /// 退出本实体之后才能执行 root 最终清理。
    pub(crate) fn request_close_(&self) -> Option<NonNull<RootScope>> {
        self.handles_
            .fetch_or(FLAG_CLOSED | FLAG_CLOSE_REQUESTED, Ordering::AcqRel);
        self.try_flush_()
    }

    /// 尝试执行延迟清盘，并在“本域已经彻底清空”时登记本域退出。
    ///
    /// 返回 `Some(root)` 表示本域是 root 树中**最后一个**完成清空的域，调用方必须在
    /// 不再持有任何指向本实体（以及本实体内存）的引用之后调用
    /// [`RootScope::finalize_`]。返回 `None` 表示无需（或不能）由本次调用触发 root 清理。
    ///
    /// # 同步
    ///
    /// - 用 `try_write` 拿域写锁：清盘路径可能与同一个域里的级联 `Retain` drop 重入，
    ///   阻塞等待会自死锁，因此抢不到锁就放弃，等下一次句柄析构 / 视图释放重试；
    /// - 进入前给 root 的 `flush_inflight_` 加一，退出时减一；root 最终清理会等它归零，
    ///   避免在别的线程还站在本实体内存上时释放实体池。
    pub(crate) fn try_flush_(&self) -> Option<NonNull<RootScope>> {
        let root = self.root_?;
        // SAFETY: root 在树内至少还有一个存活域时不会被释放；本域正是其中之一
        unsafe { root.as_ref() }.begin_flush_();
        let result = self.try_flush_inner_();
        // SAFETY: 同上；finalize_ 会等 flush_inflight_ 归零
        unsafe { root.as_ref() }.end_flush_();
        result
    }

    /// 归还槽位之后的轻量收尾：只尝试登记本域退出，**不**重新扫描弱池链。
    ///
    /// 数据已经由 `Retain::drop` 的确定性析构或此前的 `close` 清空，归还槽位本身也已经
    /// 由 [`ScopeInner::release_slot_`] 处理；这里唯一还需要做的是“弱池链清空后回收强池
    /// 并登记退出”。如果每次 `Retain` 归还都跑一遍 `flush_data_`，释放 N 个失效句柄就会
    /// 变成 O(N²) 的池链扫描。
    pub(crate) fn try_retire_only_(&self) -> Option<NonNull<RootScope>> {
        let root = self.root_?;
        // SAFETY: root 在树内至少还有一个存活域时不会被释放；本域正是其中之一
        unsafe { root.as_ref() }.begin_flush_();
        let result = self.try_enter_write_(false);
        // SAFETY: 同上；finalize_ 会等 flush_inflight_ 归零
        unsafe { root.as_ref() }.end_flush_();
        result
    }

    /// [`ScopeInner::try_flush_`] 的实体内部实现。
    fn try_flush_inner_(&self) -> Option<NonNull<RootScope>> {
        self.try_enter_write_(true)
    }

    /// 尝试拿域写锁；`flush` 为真时先按关闭状态清盘，最后尝试登记退出。
    fn try_enter_write_(&self, flush: bool) -> Option<NonNull<RootScope>> {
        let lock = self.lock_?;
        // SAFETY: lock_ 指向包含本结构的锁，实体在 root 实体池里长期存活
        let lock = unsafe { lock.as_ref() };
        let mut session = lock.acquire_session();
        let Result::Ok(mut guard) = session.try_write() else {
            return Option::None;
        };
        if flush && guard.is_close_requested_() {
            // SAFETY: 已请求关闭；flush_data_ 只强制析构 Created 状态的对象，遇到
            // Owning / Sharing 会跳过，不会让直接数据视图悬空。
            unsafe { guard.flush_data_() };
        }
        guard.try_retire_()
    }

    /// 在持有域写锁时判断本域是否已经彻底清空，并登记退出。
    ///
    /// 清空条件：
    ///
    /// 1. 没有任何 `Scope` 句柄；
    /// 2. 弱池链为空（所有槽位都已归还，因此没有任何 `Retain`）；
    /// 3. 强池链为空（数据已全部析构、强池已整链回收）。
    ///
    /// 三条同时成立时，本域不再可能被任何线程访问；此时才把 root 的 `live_scopes_`
    /// 减一。`RETIRED` 位的 CAS 保证即使多个线程先后观察到相同状态，也只减一次。
    fn try_retire_(&mut self) -> Option<NonNull<RootScope>> {
        if self.handle_count_() != 0 {
            return Option::None;
        }
        let weak_empty = self.weak_head_().is_none();
        if weak_empty && self.strong_head_.is_some() {
            // 弱池链为空意味着所有槽位都已归还，数据必然已经析构；此时强池只承载
            // 已死亡对象的 chunk 内存，可以整链回收。否则“正常析构完最后一个对象”的
            // 域会永远留着强池，root close 也就永远等不到 live_scopes_ 归零。
            // SAFETY: 没有槽位指向这些强块，数据均已析构
            unsafe { self.reclaim_strong_pools_() };
        }
        if !weak_empty || self.strong_head_.is_some() {
            return Option::None;
        }
        // 还有别的线程持有实体指针时不能登记退出：它可能在释放最后一个槽位后马上调用
        // try_flush_，若此时 root 已经清理就会悬空。只有当前线程独占时才允许退出。
        if self.users_.load(Ordering::Acquire) != 1 {
            return Option::None;
        }
        if !self.try_mark_retired_() {
            return Option::None;
        }
        let root = self.root_?;
        // SAFETY: 本域仍持有 root 的存活计数；在 live_scopes_ 归零之前 root 不会被释放
        let root_ref = unsafe { root.as_ref() };
        let prev = root_ref.live_scopes_.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "live_scopes_ 不能减到零以下");
        if prev == 1 && root_ref.is_closing_() {
            // 与 `RootScope::close` 的兜底清理选举：只有赢家负责 finalize，避免两边同时
            // 认为自己是最后一个而重复释放 root。
            if root_ref.try_elect_root_finalize_() {
                // 收尾交给调用方：它必须先完全退出本实体，才能去释放实体池
                Option::Some(root)
            } else {
                Option::None
            }
        } else {
            Option::None
        }
    }

    /// 归还一个已经死亡的槽位。
    ///
    /// 条件由调用方保证：`weak_count == 0` 且状态为 [`crate::weak_::DataState::Destroyed`]。
    /// 若这是所属池的最后一个活槽，则在弱池链锁内把池摘下，并在锁外释放池内存。
    ///
    /// # Safety
    ///
    /// `weak` 必须落在本域的某个弱池里，且已经没有任何线程会再访问它。
    ///
    /// 返回 `true` 表示本次归还把**整条弱池链清空**（最后一个池被摘下）。调用方可以据此
    /// 只在这种情况下尝试 `try_retire_only_`，避免每个失效 `Retain` 都去扫一遍池链。
    pub(crate) unsafe fn release_slot_(&self, weak: NonNull<WeakSlot<()>>) -> bool {
        let Option::Some(pool) = WeakPool::of_slot_(weak) else {
            return false;
        };
        let mut to_free = Option::None;
        let mut chain_empty = false;
        {
            let mut session = self.weak_chain_.lock_session();
            let mut chain = match session.lock().wait() {
                Result::Ok(guard) => guard,
                Result::Err(_) => unreachable!("weak chain 自旋锁不应失败"),
            };
            // SAFETY: 槽位属于本域且已死亡；池仍存活（本槽位在归还前仍计入活槽）
            let became_empty = unsafe { pool.as_ref() }.return_slot_(weak);
            if !became_empty {
                return false;
            }
            // SAFETY: 同上；仍持有链锁，摘链动作是独占的
            if unsafe { pool.as_ref() }.try_mark_detached_() {
                chain.unlink_(pool);
                chain_empty = chain.head_.is_none();
                to_free = Option::Some(pool);
            }
        }
        if let Option::Some(pool) = to_free {
            // SAFETY: 已摘链，且没有任何槽位 / 线程还指向该池
            let alloc = unsafe { self.allocator_.as_ref() };
            // SAFETY: 同上
            unsafe { WeakPool::dealloc_(pool, alloc) };
        }
        chain_empty
    }

    /// 取得一次使用租约。
    #[inline]
    pub(crate) fn incr_user_(&self) {
        self.users_.fetch_add(1, Ordering::AcqRel);
    }

    /// 归还一次使用租约。
    #[inline]
    pub(crate) fn decr_user_(&self) {
        self.users_.fetch_sub(1, Ordering::AcqRel);
    }

    #[inline]
    pub(crate) fn incr_handle_(&self) {
        let previous = self.handles_.fetch_add(1, Ordering::Relaxed);
        debug_assert!(
            previous & HANDLE_COUNT_MASK != HANDLE_COUNT_MASK,
            "Scope 句柄计数溢出到状态位"
        );
    }

    /// 减少句柄计数，返回减少前的值；返回 1 表示本次是最后一个句柄。
    ///
    /// 最后一个句柄的减一与 `CLOSED` 置位在同一次 CAS 中完成。
    pub(crate) fn decr_handle_(&self) -> usize {
        let mut current = self.handles_.load(Ordering::Acquire);
        loop {
            let count = current & HANDLE_COUNT_MASK;
            debug_assert!(count > 0, "Scope 句柄计数不能减到零以下");
            let mut next = (current & FLAG_MASK) | (count - 1);
            if count == 1 {
                next |= FLAG_CLOSED;
            }
            match self.handles_.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Result::Ok(_) => return count,
                Result::Err(actual) => current = actual,
            }
        }
    }

    /// 尝试登记“本域已退出”；只有第一次返回 `true`。
    fn try_mark_retired_(&self) -> bool {
        self.handles_.fetch_or(FLAG_RETIRED, Ordering::AcqRel) & FLAG_RETIRED == 0
    }
}

/// 固定的锁类型：`ScopeInner` 内联在 `UnsafeCell` 里，锁内核的分配器被擦成
/// `&'static dyn Allocator`，因此 `Scope` 不需要知道创建期的 `C`。
pub(crate) type ScopeLock = CooperativeRwLock<
    ScopeInner,
    usize,
    AtomicUsize,
    LocksOrderings,
    &'static (dyn Allocator + Send + Sync),
>;

/// 一个域的完整实体：锁（内含 [`ScopeInner`]）+ 每域一份的用户指定分配器。
///
/// `lock_` 是首字段，因此实体数据地址就是 `ScopeLock` 的地址；`Scope` 只保存域实体
/// `StrongChunk` 的指针，通过 chunk 的 `data_addr_()` 拿到本结构。
#[repr(C)]
pub(crate) struct ScopeEntity<A>
where
    A: Allocator,
{
    lock_: ScopeLock,
    alloc_: A,
}

impl<A> ScopeEntity<A>
where
    A: Allocator,
{
    /// 在 root 实体池里申请并就地构造一个实体，返回实体 `StrongChunk` 的首址与它的
    /// `ScopeInner` 指针。
    ///
    /// `pool` 是 root 自己的实体池；调用方必须已经持有 root 的实体池锁（这样实体分配
    /// 与 root 最终清理不会并发）。`alloc` 是每域一份的池分配器，放在实体尾部；
    /// `ScopeInner::allocator_` 指向它。
    ///
    /// # Errors
    ///
    /// root 分配器无法提供实体所需内存时返回 [`AllocError`]。
    pub(crate) fn allocate_entity_in_(
        root: &'static RootScope,
        pool: &mut ScopeInner,
        alloc: A,
        page_size: usize,
    ) -> Result<NonNull<StrongChunkBase>, AllocError>
    where
        A: Send + Sync + 'static,
    {
        // 一个实体 = 一个 StrongChunk<ScopeEntity<A>, ()>；chunk 头 + 实体数据都在 root 池里。
        let entity_layout = Layout::new::<ScopeEntity<A>>();
        let Option::Some((chunk_layout, dat_offset)) =
            StrongChunk::<ScopeEntity<A>>::layout_sized(entity_layout)
        else {
            return Result::Err(AllocError);
        };
        // 先取身份槽位，再从 root 的实体池分配 chunk；失败时把槽位还回去。
        let weak = match pool.allocate_weak_() {
            Option::Some(weak) => weak,
            Option::None => return Result::Err(AllocError),
        };
        let (mem, pool_order) = match pool.allocate_(chunk_layout) {
            Result::Ok(allocated) => allocated,
            Result::Err(err) => {
                pool.deallocate_weak_(weak);
                return Result::Err(err);
            }
        };
        // 类型表与强块头部共用同一个合成键；实体 chunk 也必须能按类型表析构。
        let _ = root.ensure_chunk_meta_::<ScopeEntity<A>, ()>();
        let base = mem.as_ptr() as *mut u8 as *mut StrongChunk<ScopeEntity<A>>;
        // SAFETY: chunk_layout / dat_offset 来自同一布局；weak 与 mem 都是刚取出的独占资源
        unsafe {
            StrongChunk::<ScopeEntity<A>>::init_sized(base, weak, pool_order, (), dat_offset)
        };
        // 实体自身的身份槽位：状态 Created、弱计数 1（代表“root 实体池持有一份”）。
        // 它不由任何 `Retain` 归还；root 最终清理会把实体池当普通域清盘，按弱槽位链
        // 找到它并用实体 vtable 析构。
        // SAFETY: weak 是本池刚取出、尚未发布的槽位
        let weak_ref = unsafe { weak.as_ref() };
        weak_ref.chunk_state().init_created();
        weak_ref.incr_weak_count();
        // SAFETY: 头部已初始化，data_ptr 落在 chunk 尾部的实体数据区
        let entity = unsafe { &mut *base }.data_ptr();
        // SAFETY: entity 指向尚未初始化的实体；alloc_ 字段地址稳定
        let alloc_ptr =
            unsafe { ptr::addr_of_mut!((*entity).alloc_) } as *mut (dyn Allocator + Send + Sync);
        // SAFETY: 地址来自刚分配的实体，非空
        let allocator = unsafe { NonNull::new_unchecked(alloc_ptr) };
        let mut inner = ScopeInner::empty_(allocator, page_size);
        inner.handles_ = AtomicUsize::new(1);
        inner.set_root_(NonNull::from(root));
        // SAFETY: entity 是刚分配、尚未写入的实体；lock_ 是首字段，地址稳定
        inner.lock_ = Option::Some(unsafe { NonNull::new_unchecked(entity.cast::<ScopeLock>()) });
        let lock = ScopeLock::new_in(inner, AtomicUsize::new(0), root.allocator_());
        // SAFETY: chunk 数据区独占；锁 / 分配器分别就地写入
        unsafe {
            ptr::write(ptr::addr_of_mut!((*entity).lock_), lock);
            ptr::write(ptr::addr_of_mut!((*entity).alloc_), alloc);
        }
        // SAFETY: chunk 头部已初始化，地址非空
        Result::Ok(unsafe { NonNull::new_unchecked(base as *mut StrongChunkBase) })
    }
}

#[cfg(test)]
mod tests_ {
    use super::*;
    use crate::scope_inner_::DEFAULT_PAGE_SIZE;
    use std::alloc::Global;

    static GLOBAL: Global = Global;

    /// 验证句柄计数与三个状态位确实打包在同一个原子字里。
    /// Verify that the handle count and three state bits are packed into one atomic word.
    ///
    /// - 手段：构造一个不带 root / 锁的空 `ScopeInner`，依次增加句柄、减到 0、调用
    ///   `request_close_`、`try_mark_retired_`，并在每一步后读回计数与状态位。
    ///   / How: build an empty `ScopeInner` without a root/lock, increment handles, decrement
    ///   to zero, call `request_close_` and `try_mark_retired_`, reading back the count and
    ///   flags after every step.
    /// - 判断：最后一次减一同时置 `CLOSED`；`request_close_` 同时置 `CLOSED | REQUESTED`；
    ///   退出登记只成功一次；所有状态变化都不改变句柄计数。
    ///   / What: the last decrement sets `CLOSED` atomically, `request_close_` sets both
    ///   `CLOSED` and `REQUESTED`, retirement succeeds only once, and no state change alters
    ///   the handle count.
    #[test]
    fn handle_word_packs_flags_and_count() {
        let allocator: NonNull<dyn Allocator + Send + Sync> = NonNull::from(&GLOBAL);
        let inner = ScopeInner::empty_(allocator, DEFAULT_PAGE_SIZE);

        assert_eq!(inner.handle_count_(), 0);
        assert!(!inner.is_closed_());
        assert!(!inner.is_close_requested_());

        inner.incr_handle_();
        inner.incr_handle_();
        assert_eq!(inner.handle_count_(), 2);
        assert_eq!(inner.decr_handle_(), 2);
        assert_eq!(inner.handle_count_(), 1);
        assert!(!inner.is_closed_(), "还有句柄时不能置 closed");

        // 最后一次减一必须同时置 CLOSED。
        assert_eq!(inner.decr_handle_(), 1);
        assert_eq!(inner.handle_count_(), 0);
        assert!(inner.is_closed_());
        assert!(!inner.is_close_requested_());

        // request_close 同时置 CLOSED 与 CLOSE_REQUESTED；没有 root / 锁，flush 是空操作。
        let _ = inner.request_close_();
        assert!(inner.is_close_requested_());
        assert_eq!(inner.handle_count_(), 0);

        assert!(inner.try_mark_retired_());
        assert!(!inner.try_mark_retired_(), "退出登记只能成功一次");
        assert_eq!(inner.handle_count_(), 0, "状态位不得污染句柄计数");

        // 再加一个句柄：状态位保持不变，计数正常递增。
        inner.incr_handle_();
        assert_eq!(inner.handle_count_(), 1);
        assert!(inner.is_closed_());
        assert!(inner.is_close_requested_());
    }
}
