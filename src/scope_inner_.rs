use core::{
    alloc::{AllocError, Allocator, Layout},
    mem::{self, MaybeUninit},
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::weak_::WeakChunk;

pub(crate) const DEFAULT_CELL_SIZE: usize = mem::size_of::<usize>();
pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub type PoolIndex = u16;

pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<ScopeInner<DEFAULT_CELL_SIZE>> = AtomicPtr::new(ptr::null_mut());

#[repr(C)]
pub(crate) struct ScopeInner<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> {
    /// 子域数量
    children_num_: AtomicUsize,
    /// 父域指针
    parent_scope_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 指向 Scope 所持有的第一块内存池
    chain_head_: Option<NonNull<ChainedPool<CELL_SIZE>>>,
    /// 指向 Scope 所持有的最新一块内存池
    chain_tail_: Option<NonNull<ChainedPool<CELL_SIZE>>>,
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

    pub fn allocate(
        &mut self,
        layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        todo!("[ScopeInner::allocate] not implemented.")
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 内嵌式内存链块，链块的头部若干(16)字节就是自身。内部存储的就是 StrongChunk。
/// 由于 PoolIndex 只有 16 位，多数情况下 CELL_SIZE 是 8，于是一个 ChainedPool
/// 的大小通常不会大于 2^(16 + 3) = 192 K Bytes。
#[repr(C)]
struct ChainedPool<const CELL_SIZE: usize> {
    /// 上一个内存链块地址
    prev_pool_: Option<NonNull<ChainedPool<CELL_SIZE>>>,
    /// 荷载可用的 cell 的数量
    cell_count_: PoolIndex,
    /// 分配已用的 cell 的数量，由于 ChainedPool 的分配方式总是从前到后。且
    /// 总是一次性全体回收。所以根据 used_count 可以计算出第一个空闲位置的地址。
    used_count_: PoolIndex,
}

impl<const CELL_SIZE: usize> ChainedPool<CELL_SIZE> {
    const THIS_SIZE: usize = mem::size_of::<Self>();

    const fn memory_(&self) -> NonNull<[u8]> {
        let p = self as *const Self as *const u8;
        unsafe {
            let data = p.add(ChainedPool::<CELL_SIZE>::THIS_SIZE);
            let len = (self.cell_count_ as usize) * CELL_SIZE;
            let slice = ptr::slice_from_raw_parts(data, len);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    const fn free_addr_(&self) -> NonNull<[u8]> {
        let p = self as *const Self as *const u8;
        let offset = (self.used_count_ as usize) * CELL_SIZE;
        unsafe {
            let data = p.add(ChainedPool::<CELL_SIZE>::THIS_SIZE + offset);
            let len = (self.cell_count_ as usize) * CELL_SIZE;
            let slice = ptr::slice_from_raw_parts(data, len);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    /// 尝试分配，若容量不足，返回最大能分配的长度
    fn allocate_(
        &mut self,
        layout: Layout,
    ) -> Result<NonNull<[u8]>, usize> {
        todo!("[ChainedBlock::allocate_] not implemented")
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 一个专门用于存储 `Retain<T>` 所需弱引用槽位（slot）的内存池。
///
/// # 布局
///
/// 与 [`ChainedPool`] 一样是内嵌式的：池头（即 `Self`）位于块首，槽位数组紧随其后，
/// 并记录在 `cell_offset_` 中。因此由任意槽位地址减去 `cell_offset_` 与池头大小，
/// 即可反推出所属池的地址；`WeakChunkState::pool_order_` 正是为此保留的槽位序号。
/// 注意池头自身也按槽位大小对齐（`size_of::<Self>()` 恰好等于 2 个槽位），所以
/// `cell_offset_` 是整除得到的槽位个数，而不是字节偏移。
///
/// 虽然带有 `CELL_SIZE` 参数，但槽位的实际存储与它无关：池中槽位一律按
/// [`WeakChunk<()>`] 解释，`WeakChunk<T>` 之于任意 `T` 只在类型上区分，尺寸恒等于
/// `WeakChunk<()>`，正是靠这一点池才能在不为每种 `T` 单独布局的前提下复用槽位。
/// 又由于索引一律是 [`PoolIndex`]（u16），池的槽位数天然被限制在 `PoolIndex::MAX`
/// 以内。
///
/// # 空闲槽位链表
///
/// 池的槽位数组自身无须任何额外元数据来记录空闲状态，空闲槽位被串成一条内嵌的
/// 虚拟链表：链接指针借用槽位内的 `WeakChunkState::next_freed_`，链表头则存放于
/// `latest_free_`，因此池只多付出一个 `PoolIndex` 的代价。
///
/// - 初始化时便把全部槽位串成一条顺序链：槽位 `i` 的 `next_freed_` 指向 `i + 1`，
///   链尾指向 `capacity_`；`latest_free_` 指向槽位 0。于是"从未分配过的槽位"无须
///   特殊对待，它只是链表中最靠后的一段。
/// - 分配即从链表头摘下一个槽位，`latest_free_` 随之推移；`pool_order_` 也在此刻
///   写入，从此这个槽位便认得自己的池与序号。
/// - 池尚未耗尽便有槽位归还：归还者成为新的链表头，其 `next_freed_` 被写成先前的
///   `latest_free_`，于是所有空闲槽位始终可从 `latest_free_` 一条链走到底。
/// - 当 `used_length_ == capacity_` 时链表已空，此时 `latest_free_` 的取值不再有意义，
///   也无须维护。
///
/// 因此分配策略是：只要池未满，就从 `latest_free_` 摘下头节点；池满则分配失败。
/// 这既保证了"后归还者先被复用"（LIFO）的局部性，也让池在反复分配/归还的稳态下
/// 不再消耗新槽位。
///
/// # 分配语义的约束
///
/// 池只负责槽位的进出，不负责槽位内容的语义。`pool_order_` 与 `next_freed_` 之外的
/// 字段（如 `WeakChunkState::weak_state_` 与指向强块的指针）属于使用者；归还槽位时
/// 池只改写 `next_freed_`，其余状态由持有该槽位的一方负责清理。
///
/// [`ChainedPool`]: ChainedPool
pub(crate) struct WeakChunkPool<const CELL_SIZE: usize> {
    // 最多可以存储多少个 WeakChunk，也就是紧随池头之后的槽位数组长度
    capacity_: PoolIndex,
    // 已经分配出去的 WeakChunk 数量，注意 WeakChunkPool 的分配顺序是不可知的
    used_length_: PoolIndex,
    // 槽位数组相对池头的偏移量（按 WeakChunk 个数计算，而非字节）
    cell_offset_: PoolIndex,
    // 空闲槽位虚拟链表的表头；仅当 used_length_ < capacity_ 时有效，
    // used_length_ == capacity_ 时链表已空，该字段不再有意义
    latest_free_: PoolIndex,

    // 同一条内存链上的兄弟池，供池容量不足时串联扩展使用
    prev_: Option<NonNull<WeakChunkPool<CELL_SIZE>>>,
    next_: Option<NonNull<WeakChunkPool<CELL_SIZE>>>,
    // 本池所属的域，槽位归还与强块回收都要回到这里
    root_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
}

impl<const CELL_SIZE: usize> WeakChunkPool<CELL_SIZE> {
    /// 一个槽位的大小，池中所有槽位一律按 `WeakChunk<()>` 解释
    const SLOT_SIZE: usize = mem::size_of::<WeakChunk<()>>();

    /// 池头自身占据的槽位个数。池头与槽位都按 `WeakChunk<()>` 对齐，
    /// 因此这里必定整除，且在 64 位平台上池头恰好等于 2 个槽位。
    const HEADER_SLOT_COUNT: usize = mem::size_of::<Self>() / Self::SLOT_SIZE;

    /// 根据目标要容纳的 WeakChunk 数量，计算最小内存占用量
    pub fn min_size_for_max_count(count: PoolIndex) -> usize {
        mem::size_of::<Self>() + (count as usize) * Self::SLOT_SIZE
    }

    /// 根据目标最大的内存占用量，计算最大能容纳多少个 WeakChunk
    pub fn max_count_within_max_size(size: usize) -> usize {
        let header = mem::size_of::<Self>();
        if size <= header {
            return 0usize;
        }
        (size - header) / Self::SLOT_SIZE
    }

    /// 用 `cell_count` 个槽位初始化一个池，并通过 `alloc` 申请内存。
    ///
    /// # Errors
    ///
    /// 当 `cell_count` 为 0，或底层分配器无法满足布局要求时返回 [`AllocError`]。
    pub fn try_new_with_cell_count(
        cell_count: PoolIndex,
        alloc: &'static dyn Allocator,
    ) -> Result<NonNull<Self>, AllocError> {
        if cell_count == 0 {
            return Result::Err(AllocError);
        }
        let layout = Self::layout_of_(cell_count);
        let mem = alloc.allocate(layout)?;
        let pool = Self::init_in_place_(mem, cell_count);
        Result::Ok(pool)
    }

    /// 计算容纳 `cell_count` 个槽位所需的布局。布局以池头对齐，同时保证紧随其后的
    /// 槽位数组也满足 `WeakChunk<()>` 的对齐要求。
    fn layout_of_(cell_count: PoolIndex) -> Layout {
        let size = Self::min_size_for_max_count(cell_count);
        // 池头与槽位同对齐，所以槽位数组的起始地址天然满足对齐要求；这里再取一次
        // 最大值只是把该前提写成布局上的显式约束。
        let align = mem::align_of::<Self>().max(mem::align_of::<WeakChunk<()>>());
        // SAFETY: size 只是池头大小加上 cell_count * 16 字节，align 不超过两者对齐的
        // 较大值（64 位平台上为 8），既不会溢出也不会违反 Layout 的约束。
        unsafe { Layout::from_size_align_unchecked(size, align) }
    }

    /// 在 `mem` 所指的内存上就地构造池头，并把全部槽位串成初始的空闲链表。
    ///
    /// 初始化后的不变量为：槽位 `i` 的 `next_freed_` 指向 `i + 1`，末槽指向
    /// `capacity_`，`latest_free_` 为 0，`used_length_` 为 0。
    ///
    /// # Safety
    ///
    /// `mem` 必须是由 `layout_of_(cell_count)` 分配、尚未被写入的独占内存块。
    fn init_in_place_(mem: NonNull<[u8]>, cell_count: PoolIndex) -> NonNull<Self> {
        let mut mem = mem;
        let base = unsafe { mem.as_mut().as_mut_ptr() as *mut Self };
        let all_slots = ptr::slice_from_raw_parts_mut(
            unsafe { (base as *mut u8).byte_add(mem::size_of::<Self>()) } as *mut WeakChunk<()>,
            cell_count as usize,
        );
        // 池头与槽位数组都在同一块刚刚分配、尚未共享出去的内存上，
        // 因此独占地写入它们是安全的。
        unsafe {
            let pool = &mut *base;
            let slots = &mut *all_slots;
            WeakChunk::<()>::init_slots(slots, cell_count);
            pool.capacity_ = cell_count;
            pool.used_length_ = 0;
            pool.cell_offset_ = Self::HEADER_SLOT_COUNT as PoolIndex;
            pool.latest_free_ = 0;
            pool.prev_ = Option::None;
            pool.next_ = Option::None;
            pool.root_ = Option::None;
        }
        // SAFETY: base 来自分配器返回的非空内存块，必然非空
        unsafe { NonNull::new_unchecked(base) }
    }

    /// 在 `max_size` 字节的预算内，取尽可能多的槽位初始化一个池。
    ///
    /// # Errors
    ///
    /// 当预算连一个槽位都放不下，或底层分配器无法满足布局要求时返回 [`AllocError`]。
    pub fn try_new_with_max_size(
        max_size: usize,
        alloc: &'static dyn Allocator,
    ) -> Result<NonNull<Self>, AllocError> {
        let count = Self::max_count_within_max_size(max_size);
        if count == 0 || count > PoolIndex::MAX as usize {
            return Result::Err(AllocError);
        }
        Self::try_new_with_cell_count(count as PoolIndex, alloc)
    }

    /// 列出所有 WeakChunk，其中可能包含未经初始化的。
    pub const fn chunks(&mut self) -> NonNull<[WeakChunk<()>]> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * mem::size_of::<WeakChunk<()>>();
        let length = self.capacity_ as usize;
        let chunks = unsafe { this.byte_add(offset) as *mut WeakChunk<()> };
        let ptr = ptr::slice_from_raw_parts_mut(chunks, length);
        unsafe { NonNull::new_unchecked(ptr) }
    }

    /// 池的槽位总容量。
    pub const fn capacity(&self) -> PoolIndex {
        self.capacity_
    }

    /// 池当前已经分配出去的槽位数量。
    pub const fn used_count(&self) -> PoolIndex {
        self.used_length_
    }

    /// 池当前的空闲槽位数量，即容量与已分配数量之差。
    pub const fn free_count(&self) -> PoolIndex {
        self.capacity_ - self.used_length_
    }

    /// 空闲槽位链表当前的表头；仅当池未满时有意义，池满时该值应被忽略。
    #[cfg(test)]
    pub const fn latest_free(&self) -> PoolIndex {
        self.latest_free_
    }

    /// 从空闲链表的表头摘下一个槽位并返回其序号；池已满时返回 [`None`]。
    ///
    /// 摘取过程只改动池头自身的 `latest_free_`：槽位的 `pool_order_` 早在池构造时就已
    /// 按数组下标写好并终生只读，这里无须也无法再动它。返回的槽位内容尚未初始化，
    /// 须由调用方填写。
    pub fn allocate(&mut self) -> Option<PoolIndex> {
        // used_length_ == capacity_ 意味着链表已空，此时 latest_free_ 不再有意义
        if self.used_length_ == self.capacity_ {
            return Option::None;
        }
        let prev_free = self.latest_free_;
        // SAFETY: 池未满时 index 必定落在 0..capacity_ 内，见上方关于链表的说明
        let slot = unsafe { self.chunk_mut_at_(prev_free) };
        self.latest_free_ = slot.next_freed();
        self.used_length_ += 1;
        Option::Some(prev_free)
    }

    /// 把一个槽位归还到空闲链表的表头。
    ///
    /// 归还只改动槽位的 `next_freed_`（指向先前的表头）与 `weak_state_`（归还时不再
    /// 残留旧的引用计数），其余内容由持有该槽位的一方在归还前清理干净。当 `index`
    /// 不是本池当前已分配的槽位时返回 `false`，池的状态不变。
    pub fn deallocate(&mut self, index: PoolIndex) -> bool {
        if index >= self.capacity_ || self.used_length_ == 0 {
            return false;
        }
        let head = self.latest_free_;
        // SAFETY: index 已按 capacity_ 校验，落在槽位数组范围内
        let slot = unsafe { self.chunk_mut_at_(index) };

        debug_assert_eq!(slot.weak_count(), 0usize);

        slot.link_as_freed(head);
        self.latest_free_ = index;
        self.used_length_ -= 1;
        true
    }

    /// 按槽位序号取槽位地址。
    ///
    /// # Safety
    ///
    /// `index` 必须小于 `capacity_`，否则返回的指针会越出槽位数组。
    unsafe fn chunk_mut_at_(&mut self, index: PoolIndex) -> &mut WeakChunk<()> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot = unsafe {
            this.byte_add(offset + index as usize * Self::SLOT_SIZE) as *mut WeakChunk<()>
        };
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        unsafe { &mut *slot }
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub(crate) type RootScopeRef<const CELL_SIZE: usize>
    = &'static ScopeInner<CELL_SIZE>;

#[repr(C)]
pub(crate) struct RootScope<A, const CELL_SIZE: usize>
where
    A: Sized + Allocator,
{
    scope_inner_: ScopeInner<CELL_SIZE>,
    allocator_: A,
    weak_chunks_: Option<NonNull<WeakChunkPool<CELL_SIZE>>>,
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
        let init_cell_count = WeakChunkPool::<CELL_SIZE>
            ::max_count_within_max_size(page_size);
        assert!(
            init_cell_count <= PoolIndex::MAX as usize,
            "init_cell_count({}) <= PoolIndex::MAX({})",
            init_cell_count, PoolIndex::MAX,
        );
        let init_cell_count = init_cell_count as PoolIndex;

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
                    return Result::Err(unsafe { &* existing });
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
        let root_scope_mut: &'static mut Self = unsafe {
            &mut * root_scope_ptr
        };
        // 这里开始手动初始化 root scope
        root_scope_mut.allocator_ = allocator;
        let alloc: &'static dyn Allocator = &mut root_scope_mut.allocator_;
        root_scope_mut.scope_inner_ = ScopeInner {
            children_num_: AtomicUsize::new(0usize),
            parent_scope_: Option::None,
            chain_head_: Option::None,
            chain_tail_: Option::None,
            allocator_: alloc,
        };
        let x = WeakChunkPool::try_new_with_max_size(page_size, alloc)
            .expect("");
        root_scope_mut.weak_chunks_ = Option::Some(x);
        // 初始化完成后才把真正的地址放入，以表示初始化已完成
        root_ptr_ref.store(
            &mut root_scope_mut.scope_inner_,
            Ordering::SeqCst,
        );
        Result::Ok(unsafe {
            let p = root_ptr_ref.load(Ordering::Relaxed);
            &*p
        })
    }

}
