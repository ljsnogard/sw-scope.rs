use core::{
    alloc::{AllocError, Allocator, Layout},
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::{
    abs_::TrEmplace,
    index_::Retain,
    strong_::{StrongChunk, StrongChunkBase, StrongPool},
    weak_::{PreDropRecord, PreDropRegistry, WeakChunk, WeakPool, meta_to_raw_, resolve_},
};

pub(crate) const DEFAULT_CELL_SIZE: usize = mem::size_of::<usize>();
pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub type PoolIndex = u16;

pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<ScopeInner<DEFAULT_CELL_SIZE>> =
    AtomicPtr::new(ptr::null_mut());

/// 一个域的内部表示。
///
/// 域持有若干 [`StrongPool`] 组成的链，池内的对象位置终身不变（见 `StrongPool` 的文档）；
/// 域静默时整条链一次性回收。
///
/// 数据对象的"存活链"挂在弱槽位上（`WeakChunk::prev_live_` / `next_live_`），这里只保存
/// 链头与链尾。弱槽位池由整棵树共享（root 分配、子域复制指针，见
/// `dev-notes/weak-20260922-1135.md` D2d 的同款理由：`RootScope` 对分配器类型泛型，
/// 在 `Scope` 这一侧已被擦除，指不到它的字段）。
#[repr(C)]
pub(crate) struct ScopeInner<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> {
    /// 子域数量
    children_num_: AtomicUsize,
    /// 父域指针
    parent_scope_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 指向 Scope 所持有的第一块内存池
    chain_head_: Option<NonNull<StrongPool<CELL_SIZE>>>,
    /// 指向 Scope 所持有的最新一块内存池
    chain_tail_: Option<NonNull<StrongPool<CELL_SIZE>>>,
    /// 存活链链头（弱槽位）
    live_head_: Option<NonNull<WeakChunk<()>>>,
    /// 存活链链尾（弱槽位）
    live_tail_: Option<NonNull<WeakChunk<()>>>,
    /// 整棵树共享的弱槽位池链头
    weak_pool_: Option<NonNull<WeakPool<CELL_SIZE, ScopeInner<CELL_SIZE>>>>,
    /// 整棵树共享的类型级 `PreDrop` 注册表（root 分配，子域复制指针，见 D2d）
    pre_drop_registry_: Option<NonNull<PreDropRegistry>>,
    /// 指向本 Scope 所有内存池的分配器，也就是 RootScope 的分配器
    /// 也可用于计算 RootScope 的地址
    allocator_: &'static dyn Allocator,
}

impl<const CELL_SIZE: usize> ScopeInner<CELL_SIZE> {
    pub const fn root_scope(&self) -> NonNull<ScopeInner<CELL_SIZE>> {
        let p = self.allocator_ as *const dyn Allocator as *const u8;
        unsafe {
            let addr = p.byte_sub(mem::size_of::<Self>());
            let inner = addr as *const Self;
            NonNull::new_unchecked(inner as *mut Self)
        }
    }

    /// 造一个子域的内部状态；分配与就地写入由 [`ScopeInner::new_child_`] 负责。
    pub(crate) fn child_of_(parent: &Self) -> Self {
        ScopeInner {
            children_num_: AtomicUsize::new(0),
            parent_scope_: Option::Some(NonNull::from(parent)),
            chain_head_: Option::None,
            chain_tail_: Option::None,
            live_head_: Option::None,
            live_tail_: Option::None,
            weak_pool_: parent.weak_pool_,
            pre_drop_registry_: parent.pre_drop_registry_,
            allocator_: parent.allocator_,
        }
    }

    /// 用本域分配器申请并就地构造一个子域，同时把父域的子域计数加 1。
    ///
    /// # Errors
    ///
    /// 分配器无法提供 `ScopeInner` 所需内存时返回 [`AllocError`]。
    pub(crate) fn new_child_(parent: NonNull<Self>) -> Result<NonNull<Self>, AllocError> {
        // SAFETY: 由调用方保证 parent 有效
        let parent_ref = unsafe { parent.as_ref() };
        let mem = parent_ref.allocator_.allocate(Layout::new::<Self>())?;
        let inner = mem.as_ptr() as *mut u8 as *mut Self;
        // SAFETY: mem 是刚分配、尚无人共享的独占内存
        unsafe { inner.write(Self::child_of_(parent_ref)) };
        parent_ref.children_num_.fetch_add(1, Ordering::AcqRel);
        // SAFETY: inner 来自成功分配，必然非空
        Result::Ok(unsafe { NonNull::new_unchecked(inner) })
    }

    /// 只统计最新的一个内存链中可分配空间大小，因为其他空间默认不会被提前释放，
    /// 因此不必统计。`Scope` 或者说所有 Arena 的使用者就是为了一次性兜底释放，
    /// 才会选用 Arena 而不是直接用智能指针。
    pub fn free_size(&self) -> usize {
        let Option::Some(f) = self.chain_tail_ else {
            return 0usize;
        };
        let b = unsafe { f.as_ref() };
        b.free_addr_().len()
    }

    /// 从强池链上分配一块满足 `layout` 的内存；尾池放不下时开新池。
    ///
    /// # Errors
    ///
    /// 需要的 cell 数超过 [`PoolIndex`] 上界，或底层分配器失败时返回 [`AllocError`]。
    pub fn allocate(&mut self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if let Option::Some(tail) = self.chain_tail_ {
            // SAFETY: 池链上的池都由本域分配器分配且仍然存活
            let pool = unsafe { tail.as_ptr().as_mut() }.ok_or(AllocError)?;
            if let Result::Ok(mem) = pool.allocate_(layout) {
                return Result::Ok(mem);
            }
        }
        // 需要新池：容量取"够放下这一块"与默认页大小的较大者
        let need = (StrongPool::<CELL_SIZE>::THIS_SIZE + layout.size()).div_ceil(CELL_SIZE) + 1;
        let cell_count = need.max(DEFAULT_PAGE_SIZE / CELL_SIZE);
        if cell_count > PoolIndex::MAX as usize {
            return Result::Err(AllocError);
        }
        let pool = StrongPool::try_new_(cell_count as PoolIndex, self.allocator_, self.chain_tail_)?;
        if self.chain_head_.is_none() {
            self.chain_head_ = Option::Some(pool);
        }
        self.chain_tail_ = Option::Some(pool);
        // SAFETY: pool 是刚初始化的独占池
        unsafe { pool.as_ptr().as_mut() }
            .ok_or(AllocError)?
            .allocate_(layout)
            .map_err(|_| AllocError)
    }

    /// 从树共享的弱池链上取一个槽位；尾池满了就开一个新池接到链尾。
    ///
    /// 返回 [`None`] 只会在底层分配器失败时发生。
    pub(crate) fn allocate_weak_(&mut self) -> Option<NonNull<WeakChunk<()>>> {
        let head = self.weak_pool_?;
        // 沿兄弟链走到尾池
        let mut tail = head;
        while let Option::Some(next) = unsafe { tail.as_ref() }.next() {
            tail = next;
        }
        let chosen = {
            let pool = unsafe { tail.as_ptr().as_mut() }?;
            if pool.free_count() != 0 {
                tail
            } else {
                // 尾池已满：开一个新池，登记所属域并接到链尾
                let new = WeakPool::try_new_with_max_size(DEFAULT_PAGE_SIZE, self.allocator_)
                    .ok()?;
                // SAFETY: new 是刚初始化的独占池
                let new_ref = unsafe { new.as_ptr().as_mut() }?;
                // SAFETY: self 是本域，池只把它存下来再原样交还
                new_ref.set_root(NonNull::from(&mut *self));
                // SAFETY: tail 是链尾，new 尚未入链
                let tail_ref = unsafe { tail.as_ptr().as_mut() }?;
                tail_ref.link_siblings(new_ref);
                new
            }
        };
        let pool = unsafe { chosen.as_ptr().as_mut() }?;
        let index = pool.allocate()?;
        let slots = pool.slots();
        // SAFETY: index < capacity_，落点必在槽位数组内
        Option::Some(unsafe {
            NonNull::new_unchecked(slots.as_ptr().cast::<WeakChunk<()>>().add(index as usize))
        })
    }

    /// 把刚分配好的弱槽位追加到存活链尾。
    pub(crate) fn push_live_(&mut self, weak: NonNull<WeakChunk<()>>) {
        // SAFETY: weak 是本域刚分配的槽位
        let weak_ref = unsafe { weak.as_ref() };
        weak_ref.set_prev_live(self.live_tail_.map_or(ptr::null_mut(), NonNull::as_ptr));
        weak_ref.set_next_live(ptr::null_mut());
        match self.live_tail_ {
            Option::Some(tail) => unsafe { tail.as_ref() }.set_next_live(weak.as_ptr()),
            Option::None => self.live_head_ = Option::Some(weak),
        }
        self.live_tail_ = Option::Some(weak);
    }

    /// 整棵树共享的类型级 `PreDrop` 注册表（只读）。
    pub(crate) fn root_registry_(&self) -> Option<&'static PreDropRegistry> {
        let root = self.root_scope();
        // SAFETY: root 是本树的 root 域，注册表由它分配并随它存活
        let root_ref = unsafe { root.as_ref() };
        match root_ref.pre_drop_registry_ {
            Option::Some(ptr) => Option::Some(unsafe { &*ptr.as_ptr() }),
            Option::None => Option::None,
        }
    }

    /// 就地构造 `value` 并交出句柄：分配弱槽位 → 分配强块 → 绑定清理登记 → 挂存活链。
    ///
    /// # Errors
    ///
    /// 弱池已满或强池分配失败时返回 [`AllocError`]。
    pub(crate) fn put_value_<T>(&mut self, value: T) -> Result<Retain<T>, AllocError> {
        self.put_value_with_(value, Option::None)
    }

    /// 同 [`ScopeInner::put_value_`]，但允许调用方指定对象级清理登记。
    ///
    /// 有效登记按"对象级 > 类型级 > 默认"选定（`resolve_`）。
    ///
    /// # Errors
    ///
    /// 弱池已满或强池分配失败时返回 [`AllocError`]。
    pub(crate) fn put_value_with_<T>(
        &mut self,
        value: T,
        object_level: Option<&'static PreDropRecord>,
    ) -> Result<Retain<T>, AllocError> {
        // 先定好记录（只读借用），再做需要 &mut self 的分配
        let record = resolve_::<T>(self.root_registry_(), object_level);
        let weak = self.allocate_weak_().ok_or(AllocError)?;
        let layout = Layout::new::<StrongChunk<T>>();
        let mem = self.allocate(layout)?;
        let chunk = mem.as_ptr() as *mut u8 as *mut StrongChunk<T>;
        // SAFETY: mem 满足 StrongChunk<T> 的布局，且本域独占
        unsafe { (*chunk).init_with_record_(weak, value, record) };
        // SAFETY: weak 是本域刚分配的槽位
        let weak_ref = unsafe { weak.as_ref() };
        weak_ref.chunk_state_.init_created();
        weak_ref.incr_weak_count();
        self.push_live_(weak);
        Result::Ok(Retain::new(weak.cast()))
    }

    /// 就地构造一个（可能 `?Sized` 的）值并交出句柄。
    ///
    /// 与 [`ScopeInner::put_value_with_`] 的区别：数据区的布局由调用方给出，构造由 `emplace`
    /// 完成，`?Sized` 元数据由 `emplace.meta_()` 提供——这是解开"arena 要构造胖指针才能调
    /// `emplace`、而元数据本该由这次调用给出"循环的关键。
    ///
    /// # Safety
    ///
    /// 调用方必须满足 [`TrEmplace::emplace`] 的安全契约。
    pub(crate) unsafe fn emplace_value_with_<TyEmp>(
        &mut self,
        layout: Layout,
        emplace: TyEmp,
        object_level: Option<&'static PreDropRecord>,
    ) -> Result<Retain<TyEmp::Target>, AllocError>
    where
        TyEmp: TrEmplace,
    {
        let meta: <TyEmp::Target as ptr::Pointee>::Metadata = emplace.meta_().into();
        let record = resolve_::<TyEmp::Target>(self.root_registry_(), object_level);
        let weak = self.allocate_weak_().ok_or(AllocError)?;
        // StrongChunk<T> = { base_: StrongChunkBase, data_: T }；Layout::extend 给出与
        // repr(C) 一致的整块布局与数据区偏移
        let (chunk_layout, data_offset) = Layout::new::<StrongChunkBase>()
            .extend(layout)
            .map_err(|_| AllocError)?;
        let mem = self.allocate(chunk_layout)?;
        let base = mem.as_ptr() as *mut u8;
        // SAFETY: data_offset 由 Layout::extend 给出，落在这块刚分配的内存内
        let data = unsafe { base.add(data_offset) };
        // arena 先建好胖指针，再交回 emplace 就地写入
        let place = ptr::from_raw_parts_mut::<TyEmp::Target>(data.cast::<()>(), meta);
        // SAFETY: 布局匹配、数据尚未初始化，且由调用方保证 emplace 的契约
        unsafe { emplace.emplace(layout, place) };
        // 绑定：base 就是 StrongChunk<Target> 的首址
        let chunk = ptr::from_raw_parts_mut::<StrongChunk<TyEmp::Target>>(base.cast::<()>(), meta);
        // SAFETY: 数据刚由 emplace 就地构造完毕；weak 是本域刚分配的槽位
        unsafe { (*chunk).bind_fresh_(weak, meta_to_raw_(meta), record) };
        // SAFETY: weak 是本域刚分配的槽位
        let weak_ref = unsafe { weak.as_ref() };
        weak_ref.chunk_state_.init_created();
        weak_ref.incr_weak_count();
        self.push_live_(weak);
        Result::Ok(Retain::new(weak.cast()))
    }

    /// 整块回收强池链：把每个池的内存还给分配器。
    ///
    /// 只在域静默（没有存活对象、也没有子域）时调用；调用方负责在此之前完成 `PreDrop`
    /// 与数据析构。
    pub(crate) fn reclaim_strong_pools_(&mut self) {
        let mut cursor = self.chain_tail_;
        while let Option::Some(pool) = cursor {
            // SAFETY: 池仍由本域持有
            let pool_ref = unsafe { pool.as_ref() };
            cursor = pool_ref.prev_();
            // SAFETY: 池是按 `layout_for_` 分配的，且此时仍未被回收
            unsafe {
                self.allocator_.deallocate(
                    pool.cast::<u8>(),
                    StrongPool::<CELL_SIZE>::layout_for_(pool_ref.cell_count_()),
                )
            };
        }
        self.chain_head_ = Option::None;
        self.chain_tail_ = Option::None;
    }

    /// 由槽位反查它所属的弱池与池内序号。
    fn weak_pool_of_(
        &self,
        weak: NonNull<WeakChunk<()>>,
    ) -> Option<(
        NonNull<WeakPool<CELL_SIZE, ScopeInner<CELL_SIZE>>>,
        PoolIndex,
    )> {
        let mut cursor = self.weak_pool_;
        while let Option::Some(pool) = cursor {
            // SAFETY: 池链上的池都由树持有且仍然存活
            let pool_ref = unsafe { pool.as_ref() };
            if let Option::Some(index) = pool_ref.index_of_(weak) {
                return Option::Some((pool, index));
            }
            cursor = pool_ref.next();
        }
        Option::None
    }

    /// 清盘的保证路径（来源 (b)）：遍历存活链，把**仍然活着**的对象逐个 `PreDrop` + 析构，
    /// 把槽位还回所属弱池，然后整块回收强池。
    ///
    /// 与来源 (a) 共用同一个 `try_claim_destroy` 认领 CAS：级联析构或并发 `drop` 已经处理过
    /// 的槽位会认领失败，这里直接跳过，因此"恰好一次"仍然成立。
    ///
    /// # Safety
    ///
    /// 调用后本域中所有对象的句柄都会悬空；调用方必须保证此后不再使用它们。
    pub(crate) unsafe fn flush_(&mut self) {
        let mut cursor = self.live_head_;
        while let Option::Some(weak) = cursor {
            // SAFETY: 链上的槽位都由本域分配且仍然有效
            let weak_ref = unsafe { weak.as_ref() };
            // 先把后继取出来，因为下面会把本槽位还给池（会清掉链接）
            cursor = NonNull::new(weak_ref.next_live());
            if weak_ref.chunk_state_.try_claim_destroy().is_some() {
                // SAFETY: 刚由本调用认领成功，恰好执行一次
                unsafe { weak_ref.drop_data() };
                weak_ref.chunk_state_.mark_destroyed();
            }
            weak_ref.chunk_state_.mark_finalized();
            // 归还槽位：反查所属池 -> deallocate 会把状态、链接与登记一并重置
            if let Option::Some((pool, index)) = self.weak_pool_of_(weak) {
                // SAFETY: pool 由本树持有且存活
                if let Option::Some(pool_ref) = unsafe { pool.as_ptr().as_mut() } {
                    let _ = pool_ref.deallocate(index);
                }
            }
        }
        self.live_head_ = Option::None;
        self.live_tail_ = Option::None;
        self.reclaim_strong_pools_();
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub(crate) type RootScopeRef<const CELL_SIZE: usize> = &'static ScopeInner<CELL_SIZE>;

#[repr(C)]
pub(crate) struct RootScope<A, const CELL_SIZE: usize>
where
    A: Sized + Allocator,
{
    scope_inner_: ScopeInner<CELL_SIZE>,
    allocator_: A,
    weak_pools_: Option<NonNull<crate::weak_::WeakPool<CELL_SIZE, ScopeInner<CELL_SIZE>>>>,
}

impl<A, const CELL_SIZE: usize> RootScope<A, CELL_SIZE>
where
    A: Sized + Allocator,
{
    /// 并发地尝试初始化一个 RootScope，如果初始化成功返回 Ok，否则返回 Err 并携带
    /// 被其他调用者初始化的 Root.
    pub(crate) fn try_init_default_root_scope_(
        root_ptr_ref: &'static AtomicPtr<ScopeInner<CELL_SIZE>>,
        page_size: usize,
        allocator: A,
    ) -> Result<RootScopeRef<CELL_SIZE>, RootScopeRef<CELL_SIZE>>
    where
        A: 'static,
    {
        let init_cell_count = crate::weak_::WeakPool::<CELL_SIZE, ScopeInner<CELL_SIZE>>
            ::max_count_within_max_size(page_size);
        assert!(
            init_cell_count <= PoolIndex::MAX as usize,
            "init_cell_count({}) <= PoolIndex::MAX({})",
            init_cell_count,
            PoolIndex::MAX,
        );

        // 用这个绝不合法的地址是为了表明抢占中的状态
        let acquired = root_ptr_ref as *const AtomicPtr<_>
            as *mut AtomicPtr<ScopeInner<CELL_SIZE>>
            as *mut ScopeInner<CELL_SIZE>;
        let expected = ptr::null_mut();
        loop {
            let x = root_ptr_ref.compare_exchange_weak(
                expected,
                acquired,
                Ordering::Release,
                Ordering::Relaxed,
            );
            if let Result::Err(existing) = x {
                // 抢占中或者未初始化，都必须忙等以得到初始化的结果
                if existing == acquired || existing.is_null() {
                    continue;
                } else {
                    return Result::Err(unsafe { &*existing });
                }
            } else {
                break;
            }
        }
        // 这里开始处理抢占成功后的分配内存

        let mut addr = allocator
            .allocate(Layout::new::<RootScope<A, CELL_SIZE>>())
            .expect("Allocator failed in mem alloc.");
        let root_scope_ptr = unsafe {
            let slice_mut = addr.as_mut();
            &mut slice_mut[0] as *mut u8 as *mut Self
        };
        let root_scope_mut: &'static mut Self = unsafe { &mut *root_scope_ptr };
        // 这里开始手动初始化 root scope
        root_scope_mut.allocator_ = allocator;
        let alloc: &'static dyn Allocator = &mut root_scope_mut.allocator_;
        root_scope_mut.scope_inner_ = ScopeInner {
            children_num_: AtomicUsize::new(0usize),
            parent_scope_: Option::None,
            chain_head_: Option::None,
            chain_tail_: Option::None,
            live_head_: Option::None,
            live_tail_: Option::None,
            weak_pool_: Option::None,
            pre_drop_registry_: Option::None,
            allocator_: alloc,
        };
        let x = crate::weak_::WeakPool::try_new_with_max_size(page_size, alloc).expect("");
        root_scope_mut.weak_pools_ = Option::Some(x);
        // 让 `ScopeInner` 也能直接到达弱池：`Scope` 只持有 `ScopeInner`，而 `RootScope`
        // 对分配器类型泛型、在这一侧已擦除，指不到 `weak_pools_` 字段。
        root_scope_mut.scope_inner_.weak_pool_ = Option::Some(x);
        // 类型级 PreDrop 注册表：由 root 分配，子域复制指针共享同一份（D2d）
        let registry_mem = alloc
            .allocate(Layout::new::<PreDropRegistry>())
            .expect("分配 PreDrop 注册表失败");
        let registry_ptr = registry_mem.as_ptr() as *mut u8 as *mut PreDropRegistry;
        // SAFETY: registry_mem 是刚分配、尚无人共享的独占内存；new_ 不做任何分配
        unsafe { registry_ptr.write(PreDropRegistry::new_()) };
        root_scope_mut.scope_inner_.pre_drop_registry_ =
            Option::Some(unsafe { NonNull::new_unchecked(registry_ptr) });
        // 初始化完成后才把真正的地址放入，以表示初始化已完成
        root_ptr_ref.store(&mut root_scope_mut.scope_inner_, Ordering::SeqCst);
        Result::Ok(unsafe {
            let p = root_ptr_ref.load(Ordering::Relaxed);
            &*p
        })
    }
}
