use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use atomex::LocksOrderings;
use atomic_sync::rwlock::cooperative_arena::CooperativeRwLock;

use crate::{
    scope_inner_::RootScope,
    strong_::{StrongChunk, StrongChunkBase, StrongPool},
    weak_::{WeakPool, WeakSlot},
    x_deps::{atomex, atomic_sync},
};

/// 一个域的内部表示。它总是与一个 Allocator 一起分配，组成 (ScopeInner, Allocator)。
///
/// 新设计里域实体作为 `StrongChunk` 存放在 root 自己的强块池里，因此不再保存
/// 父域 / 子域 / 兄弟链；这些字段的旧版本见文件末尾注释以及 `life_` / `teardown_` 的源码。
///
/// 域持有若干 [`StrongPool`] 组成的链，池内的对象位置终身不变（见 `StrongPool` 的文档）；
/// 域静默时整条链一次性回收。弱槽位池（[`WeakPool`]）同样按域持有，因此每个域的
/// 存活槽位只在本域的池链上。
///
/// 对象的身份与生命周期状态保存在弱槽位里；存活链不逐槽位维护指针，而是由
/// [`crate::weak_::WeakSlotState::for_each_live_slot_`] 从池链与槽位状态反推。
#[repr(C)]
pub(crate) struct ScopeInner {
    /// 域标志位：`CLOSED` / `HANDLE_DROPPED`。
    pub(super) flags_: AtomicUsize,

    /// 本域弱池链的链头；弱池由每个域自己持有，不再挂在 root 上。
    pub(super) weak_head_: Option<NonNull<WeakPool>>,

    /// 本域弱池链的链尾；分配新槽位时先从尾池摘取。
    pub(super) weak_tail_: Option<NonNull<WeakPool>>,

    /// 指向 Scope 所持有的第一块强池。
    pub(super) strong_head_: Option<NonNull<StrongPool>>,

    /// 指向 Scope 所持有的最新一块强池。
    pub(super) strong_tail_: Option<NonNull<StrongPool>>,

    /// 指向本实体尾部的分配器值。
    ///
    /// 每域一份分配器，因此这里不能是 `&'static`；指针在实体落位后指向同一实体里的
    /// `alloc_` 字段，实体的字段顺序保证 `ScopeInner` 先于分配器析构。
    pub(super) allocator_: NonNull<dyn Allocator + Send + Sync>,

    /// 开新弱池 / 强池时使用的页大小；由创建期的 `C::PageSize` 决定。
    pub(super) page_size_: usize,
}

// SAFETY: 池链只在持有实体写锁时访问；弱槽位对 `Retain` 的并发访问走自身的原子状态；
// 分配器要求 `Send + Sync`。实体一旦发布就不再移动。
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
            flags_: AtomicUsize::new(0),
            weak_head_: Option::None,
            weak_tail_: Option::None,
            strong_head_: Option::None,
            strong_tail_: Option::None,
            allocator_: allocator,
            page_size_: page_size,
        }
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

/// 实体中与 `A` 无关的公共头部。
///
/// `lock_` 放在首位，于是 `Scope` 持有的 `NonNull<ScopeLock>` 与 `NonNull<ScopeHeader>`
/// 地址相同；需要句柄计数、root 或树键时可以直接回到头部。
#[repr(C)]
pub(crate) struct ScopeHeader {
    lock_: ScopeLock,
    handles_: AtomicUsize,
    closed_: AtomicBool,
    root_: &'static RootScope,
    /// 本实体所在的强块与身份槽位，供未来把 chunk / slot 还给 root 池。
    chunk_: NonNull<StrongChunkBase>,
    weak_: NonNull<WeakSlot<()>>,
    /// 单态化的析构入口：root 不需要知道 `A`，经此指针即可析构异质实体。
    drop_: unsafe fn(NonNull<ScopeHeader>),
}

/// 一个域的完整实体：固定头部 + 每域一份的用户指定分配器。
#[repr(C)]
pub(crate) struct ScopeEntity<A>
where
    A: Allocator,
{
    header_: ScopeHeader,
    alloc_: A,
}

impl ScopeHeader {
    /// 用 root 的分配器申请并就地构造一个实体。
    ///
    /// `alloc` 是每域一份的池分配器，放在实体尾部；`ScopeInner::allocator_` 指向它，
    /// 因此不会为分配器再单独做一次堆分配。
    ///
    /// # Errors
    ///
    /// root 分配器无法提供实体所需内存时返回 [`AllocError`]。
    pub(crate) fn allocate_entity_<A>(
        root: &'static RootScope,
        alloc: A,
        page_size: usize,
    ) -> Result<NonNull<Self>, AllocError>
    where
        A: Allocator + Send + Sync + 'static,
    {
        // 一个实体 = 一个 StrongChunk<ScopeEntity<A>, ()>；chunk 头 + 实体数据都在 root 池里。
        let entity_layout = Layout::new::<ScopeEntity<A>>();
        let Option::Some((chunk_layout, dat_offset)) =
            StrongChunk::<ScopeEntity<A>>::layout_sized(entity_layout)
        else {
            return Result::Err(AllocError);
        };
        // 先取身份槽位，再从 root 的实体池分配 chunk；失败时把槽位还回去。
        let allocated: Result<_, AllocError> = root.with_entity_pool_(|pool| {
            let weak = match pool.allocate_weak_() {
                Option::Some(weak) => weak,
                Option::None => return Result::Err(AllocError),
            };
            match pool.allocate_(chunk_layout) {
                Result::Ok((mem, order)) => Result::Ok((weak, mem, order)),
                Result::Err(err) => {
                    pool.deallocate_weak_(weak);
                    Result::Err(err)
                }
            }
        });
        let (weak, mem, pool_order) = allocated?;
        let base = mem.as_ptr() as *mut u8 as *mut StrongChunk<ScopeEntity<A>>;
        // SAFETY: chunk_layout / dat_offset 来自同一布局；weak 与 mem 都是刚取出的独占资源
        unsafe {
            StrongChunk::<ScopeEntity<A>>::init_sized(base, weak, pool_order, (), dat_offset)
        };
        // SAFETY: 头部已初始化，data_ptr 落在 chunk 尾部的实体数据区
        let entity = unsafe { &mut *base }.data_ptr();
        // SAFETY: entity 指向尚未初始化的实体；alloc_ 字段地址稳定
        let alloc_ptr =
            unsafe { ptr::addr_of_mut!((*entity).alloc_) } as *mut (dyn Allocator + Send + Sync);
        // SAFETY: 地址来自刚分配的实体，非空
        let allocator = unsafe { NonNull::new_unchecked(alloc_ptr) };
        let inner = ScopeInner::empty_(allocator, page_size);
        let lock = ScopeLock::new_in(inner, AtomicUsize::new(0), root.allocator_());
        // SAFETY: chunk 数据区独占；头 / 分配器分别就地写入
        unsafe {
            ptr::write(
                ptr::addr_of_mut!((*entity).header_),
                ScopeHeader {
                    lock_: lock,
                    handles_: AtomicUsize::new(1),
                    closed_: AtomicBool::new(false),
                    root_: root,
                    chunk_: NonNull::new_unchecked(base as *mut StrongChunkBase),
                    weak_: weak,
                    drop_: drop_scope_entity_::<A>,
                },
            );
            ptr::write(ptr::addr_of_mut!((*entity).alloc_), alloc);
            Result::Ok(NonNull::new_unchecked(entity.cast::<ScopeHeader>()))
        }
    }

    #[inline]
    pub(crate) fn lock_(&self) -> &ScopeLock {
        &self.lock_
    }

    /// 最后一个 `Scope` 句柄是否已经析构。
    #[inline]
    pub(crate) fn is_closed_(&self) -> bool {
        self.closed_.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn mark_closed_(&self) {
        self.closed_.store(true, Ordering::Release);
    }

    #[inline]
    pub(crate) fn incr_handle_(&self) {
        self.handles_.fetch_add(1, Ordering::Relaxed);
    }

    /// 减少句柄计数，返回减少前的值；返回 1 表示本次是最后一个句柄。
    #[inline]
    pub(crate) fn decr_handle_(&self) -> usize {
        self.handles_.fetch_sub(1, Ordering::AcqRel)
    }

    #[inline]
    pub(crate) fn root_(&self) -> &'static RootScope {
        self.root_
    }
}

/// 单态化析构入口：root 只需要 `ScopeHeader` 里的一个函数指针，就能析构异质的
/// `ScopeEntity<A>`。chunk / 弱槽位的归还留给 root 的回收路径（当前实验尚未接入）。
///
/// # Safety
///
/// `header` 必须指向一个仍然存活、且尚未析构的 `ScopeEntity<A>`。
unsafe fn drop_scope_entity_<A>(header: NonNull<ScopeHeader>)
where
    A: Allocator + Send + Sync + 'static,
{
    // SAFETY: ScopeHeader 是 ScopeEntity<A> 的首字段
    unsafe { ptr::drop_in_place(header.as_ptr() as *mut ScopeEntity<A>) };
}
