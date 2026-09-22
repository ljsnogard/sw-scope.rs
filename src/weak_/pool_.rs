use core::{
    alloc::{AllocError, Allocator, Layout},
    mem,
    ptr::NonNull,
};

use crate::{scope_inner_::PoolIndex, weak_::WeakChunk};

/// 一个专门用于存储 `Retain<T>` 所需弱引用槽位（slot）的内存池。
///
/// # 布局
///
/// 与 [`crate::strong_::StrongPool`] 一样是内嵌式的：池头（即 `Self`）位于块首，槽位数组
/// 紧随其后，并记录在 `cell_offset_` 中。因此由任意槽位地址减去 `cell_offset_` 与池头
/// 大小，即可反推出所属池的地址；`WeakChunkState::pool_order_` 正是为此保留的槽位序号。
/// `cell_offset_` 记录的是槽位数组相对池头的**槽位个数**（向上取整），因此槽位数组的
/// 起点始终按 `WeakChunk<()>` 对齐；它不假设池头大小恰好是槽位大小的整数倍。
///
/// 虽然带有 `CELL_SIZE` 参数，但槽位的实际存储与它无关：池中槽位一律按
/// [`WeakChunk<()>`] 解释，`WeakChunk<T>` 之于任意 `T` 只在类型上区分，尺寸恒等于
/// `WeakChunk<()>`，正是靠这一点池才能在不为每种 `T` 单独布局的前提下复用槽位。
/// 又由于索引一律是 [`PoolIndex`]（u16），池的槽位数天然被限制在 `PoolIndex::MAX`
/// 以内。
///
/// `Root` 是本池所属域的类型，池只负责把它存下来再原样交还，不关心它是什么。
///
/// # 空闲槽位链表
///
/// 池的槽位数组自身无须任何额外元数据来记录空闲状态，空闲槽位被串成一条内嵌的虚拟链表：
/// 链接指针借用槽位内的 `WeakChunkState::next_freed_`，链表头则存放于 `latest_free_`，
/// 因此池只多付出一个 `PoolIndex` 的代价。
///
/// - 初始化时便把全部槽位串成一条顺序链：槽位 `i` 的 `next_freed_` 指向 `i + 1`，
///   链尾指向 `capacity_`；`latest_free_` 指向槽位 0。于是"从未分配过的槽位"无须特殊
///   对待，它只是链表中最靠后的一段。
/// - 分配即从链表头摘下一个槽位，`latest_free_` 随之推移；`pool_order_` 早在构造期就
///   按数组下标写死，此处不再触碰。
/// - 池尚未耗尽便有槽位归还：归还者成为新的链表头，其 `next_freed_` 被写成先前的
///   `latest_free_`，于是所有空闲槽位始终可从 `latest_free_` 一条链走到底。
/// - 当 `used_length_ == capacity_` 时链表已空，此时 `latest_free_` 的取值不再有意义，
///   也无须维护。
///
/// # 槽位何时归还
///
/// 归还条件不是"弱引用计数归零"这么单一，而要综合槽位状态判断：只有"没有任何 `Retain`"
/// 且"数据已经析构（或从未需要析构）"时，槽位才会回到空闲链。判断依据见
/// [`crate::weak_::DataState`]。
pub(crate) struct WeakPool<const CELL_SIZE: usize, Root> {
    // 最多可以存储多少个 WeakChunk，也就是紧随池头之后的槽位数组长度
    capacity_: PoolIndex,
    // 已经分配出去的 WeakChunk 数量，注意 WeakPool 的分配顺序是不可知的
    used_length_: PoolIndex,
    // 槽位数组相对池头的偏移量（按 WeakChunk 个数计算，而非字节）
    cell_offset_: PoolIndex,
    // 空闲槽位虚拟链表的表头；仅当 used_length_ < capacity_ 时有效，
    // used_length_ == capacity_ 时链表已空，该字段不再有意义
    latest_free_: PoolIndex,

    // 同一条内存链上的兄弟池，供池容量不足时串联扩展使用
    prev_: Option<NonNull<WeakPool<CELL_SIZE, Root>>>,
    next_: Option<NonNull<WeakPool<CELL_SIZE, Root>>>,
    // 本池所属的域
    root_: Option<NonNull<Root>>,
}

impl<const CELL_SIZE: usize, Root> WeakPool<CELL_SIZE, Root> {

    /// 根据目标要容纳的 WeakChunk 数量，计算最小内存占用量
    pub fn min_size_for_max_count(count: PoolIndex) -> usize {
        Self::HEADER_SLOT_COUNT * Self::SLOT_SIZE + (count as usize) * Self::SLOT_SIZE
    }

    /// 根据目标最大的内存占用量，计算最大能容纳多少个 WeakChunk
    pub fn max_count_within_max_size(size: usize) -> usize {
        let header = Self::HEADER_SLOT_COUNT * Self::SLOT_SIZE;
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

    /// 池的槽位总容量。
    pub const fn capacity(&self) -> PoolIndex {
        self.capacity_
    }

    /// 池当前已经分配出去的槽位数量。
    pub const fn used_count(&self) -> PoolIndex {
        self.used_length_
    }

    /// 池当前的空闲槽位数量。
    pub const fn free_count(&self) -> PoolIndex {
        self.capacity_ - self.used_length_
    }

    /// 空闲槽位链表当前的表头；仅当池未满时有意义。
    #[cfg(test)]
    pub const fn latest_free(&self) -> PoolIndex {
        self.latest_free_
    }

    /// 读取某个槽位在空闲链上的后继序号，链尾为 `capacity_`。
    ///
    /// 仅供测试与诊断遍历空闲链使用。
    #[cfg(test)]
    pub fn next_freed_of(&self, index: PoolIndex) -> PoolIndex {
        // SAFETY: 由调用方保证 index 小于 capacity_
        unsafe { self.slot_at_(index) }.next_freed()
    }

    /// 直接改写某个槽位在空闲链上的后继。
    ///
    /// 仅供测试故意制造损坏的链表，用来验证不变式检查器真的有效。
    #[cfg(test)]
    pub fn set_next_freed_of(&mut self, index: PoolIndex, next: PoolIndex) {
        // SAFETY: 由调用方保证 index 小于 capacity_
        let slot = unsafe { self.slot_mut_at_(index) };
        slot.chunk_state_mut().set_next_freed(next);
    }

    /// 池链上的下一个池。
    pub const fn next(&self) -> Option<NonNull<WeakPool<CELL_SIZE, Root>>> {
        self.next_
    }

    /// 池链上的上一个池。
    pub const fn prev(&self) -> Option<NonNull<WeakPool<CELL_SIZE, Root>>> {
        self.prev_
    }

    /// 本池所属的域。
    pub const fn root(&self) -> Option<NonNull<Root>> {
        self.root_
    }

    /// 绑定本池所属的域。
    pub fn set_root(&mut self, root: NonNull<Root>) {
        self.root_ = Option::Some(root);
    }

    /// 把 `next` 接到本池之后，并回填其反向指针，从而把两个池串成双向链的一环。
    pub fn link_siblings(&mut self, next: &mut WeakPool<CELL_SIZE, Root>) {
        let this = NonNull::from(&mut *self);
        let next_ptr = NonNull::from(&mut *next);
        self.next_ = Option::Some(next_ptr);
        next.prev_ = Option::Some(this);
    }

    /// 从空闲链表的表头摘下一个槽位并返回其序号；池已满时返回 [`None`]。
    ///
    /// 摘取过程只改动池头自身的 `latest_free_`：槽位的 `pool_order_` 早在池构造时就已按
    /// 数组下标写好并终生只读，这里无须也无法再动它。返回的槽位内容尚未初始化，须由
    /// 调用方填写。
    pub fn allocate(&mut self) -> Option<PoolIndex> {
        // used_length_ == capacity_ 意味着链表已空，此时 latest_free_ 不再有意义
        if self.used_length_ == self.capacity_ {
            return Option::None;
        }
        let index = self.latest_free_;
        // SAFETY: 池未满时 index 必定落在 0..capacity_ 内，见上方关于链表的说明
        let slot = unsafe { self.slot_mut_at_(index) };
        self.latest_free_ = slot.next_freed();
        self.used_length_ += 1;
        Option::Some(index)
    }

    /// 把一个槽位归还到空闲链表的表头。
    ///
    /// 归还只重置槽位的空闲链接、状态、计数与析构登记；"数据是否已经析构"由调用方在
    /// 归还之前判断并完成。当 `index` 不是本池当前已分配的槽位时返回 `false`，池的状态
    /// 不变。
    pub fn deallocate(&mut self, index: PoolIndex) -> bool {
        if index >= self.capacity_ || self.used_length_ == 0 {
            return false;
        }
        let head = self.latest_free_;
        // SAFETY: index 已按 capacity_ 校验，落在槽位数组范围内
        let slot = unsafe { self.slot_mut_at_(index) };
        slot.link_as_freed(head);
        self.latest_free_ = index;
        self.used_length_ -= 1;
        true
    }

    /// 读取某个槽位记录的所属池序号。
    pub fn pool_order_of(&self, index: PoolIndex) -> PoolIndex {
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        let slot = unsafe { self.slot_at_(index) };
        slot.pool_order()
    }

    /// 列出全部槽位，其中可能包含尚未使用的。
    pub fn slots(&mut self) -> NonNull<[WeakChunk<()>]> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let chunks = unsafe { this.byte_add(offset) as *mut WeakChunk<()> };
        let ptr = core::ptr::slice_from_raw_parts_mut(chunks, self.capacity_ as usize);
        unsafe { NonNull::new_unchecked(ptr) }
    }
    /// 一个槽位的大小，池中所有槽位一律按 `WeakChunk<()>` 解释
    const SLOT_SIZE: usize = mem::size_of::<WeakChunk<()>>();

    /// 池头占据的槽位数：向上取整，保证槽位数组起点仍满足 `WeakChunk<()>` 的对齐要求。
    ///
    /// 池头随 `Root` 等参数变大时，这里会自然跟着变，不再依赖"池头恰好等于若干个槽位"
    /// 这种脆弱假设。
    const HEADER_SLOT_COUNT: usize =
        mem::size_of::<Self>().div_ceil(Self::SLOT_SIZE);

    /// 池头占用的字节数，即槽位数组相对池首的固定偏移。
    ///
    /// `cell_offset_` 在构造时就写成 `HEADER_SLOT_COUNT`，因此第 `i` 个槽位相对池首的偏移是
    /// `HEADER_BYTES + i * SLOT_SIZE`；这正是"由槽位地址反推所属池"能成立的前提，见
    /// [`WeakPool::of_slot_`]。
    pub(crate) const HEADER_BYTES: usize = Self::HEADER_SLOT_COUNT * Self::SLOT_SIZE;

    /// 由槽位地址反推它所属的池（O(1)，不依赖池链）。
    ///
    /// 槽位数组紧跟在池头之后，第 `i` 个槽位相对池首的偏移是
    /// `(HEADER_SLOT_COUNT + i) * SLOT_SIZE`；`i` 正是槽位里的 `pool_order_`——它在池构造时
    /// 按数组下标写死、终生只读（归还槽位也不会重置），因此对空闲槽位同样有效。
    ///
    /// 返回 `None` 表示该地址不可能是本类池的槽位（越界或未按池对齐）。
    pub(crate) fn of_slot_(weak: NonNull<WeakChunk<()>>) -> Option<NonNull<Self>> {
        let addr = weak.as_ptr() as usize;
        // 先按槽位对齐校验，避免对错位地址解引用去读 `pool_order_`
        if !addr.is_multiple_of(mem::align_of::<WeakChunk<()>>()) {
            return Option::None;
        }
        // SAFETY: addr 已按槽位对齐；调用方保证它指向一个本类池分配出的（含空闲）槽位
        let order = unsafe { weak.as_ref() }.pool_order() as usize;
        let offset = Self::HEADER_BYTES + order * Self::SLOT_SIZE;
        if addr < offset {
            return Option::None;
        }
        let base = addr - offset;
        if !base.is_multiple_of(mem::align_of::<Self>()) {
            return Option::None;
        }
        // SAFETY: base 由非空槽位地址减去固定偏移得到，必定非空
        Option::Some(unsafe { NonNull::new_unchecked(base as *mut Self) })
    }

    /// 计算容纳 `cell_count` 个槽位所需的布局。
    fn layout_of_(cell_count: PoolIndex) -> Layout {
        let size = Self::HEADER_SLOT_COUNT * Self::SLOT_SIZE
            + (cell_count as usize) * Self::SLOT_SIZE;
        let align = mem::align_of::<Self>().max(mem::align_of::<WeakChunk<()>>());
        // SAFETY: size 只是池头大小加上 cell_count * 槽位大小，align 不超过两者对齐的
        // 较大值，既不会溢出也不会违反 Layout 的约束。
        unsafe { Layout::from_size_align_unchecked(size, align) }
    }

    /// 在 `mem` 所指的内存上就地构造池头，并把全部槽位串成初始的空闲链表。
    ///
    /// # Safety
    ///
    /// `mem` 必须是由 `layout_of_(cell_count)` 分配、尚未被写入的独占内存块。
    fn init_in_place_(mem: NonNull<[u8]>, cell_count: PoolIndex) -> NonNull<Self> {
        let mut mem = mem;
        let base = unsafe { mem.as_mut().as_mut_ptr() as *mut Self };
        // 槽位数组的起点必须与 slot_at_ / slots() 用同一套算法（按槽位个数向上取整），
        // 否则"写入"与"读取"会落在两块不同的地址上。
        let header_bytes = Self::HEADER_SLOT_COUNT * Self::SLOT_SIZE;
        let all_slots = core::ptr::slice_from_raw_parts_mut(
            unsafe { (base as *mut u8).byte_add(header_bytes) } as *mut WeakChunk<()>,
            cell_count as usize,
        );
        // 池头与槽位数组都在同一块刚刚分配、尚未共享出去的内存上，因此独占地写入是安全的。
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

    /// 按槽位序号取只读槽位引用。
    ///
    /// # Safety
    ///
    /// `index` 必须小于 `capacity_`，否则返回的引用会越出槽位数组。
    unsafe fn slot_at_(&self, index: PoolIndex) -> &WeakChunk<()> {
        let this = self as *const Self as *const u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot =
            unsafe { this.byte_add(offset + index as usize * Self::SLOT_SIZE) as *const WeakChunk<()> };
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        unsafe { &*slot }
    }

    /// 按槽位序号取可写槽位引用。
    ///
    /// # Safety
    ///
    /// `index` 必须小于 `capacity_`，否则返回的引用会越出槽位数组。
    unsafe fn slot_mut_at_(&mut self, index: PoolIndex) -> &mut WeakChunk<()> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot =
            unsafe { this.byte_add(offset + index as usize * Self::SLOT_SIZE) as *mut WeakChunk<()> };
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        unsafe { &mut *slot }
    }

    /// 判断某个槽位是否落在本池的槽位数组内；是则返回它在池内的序号。
    ///
    /// 供清盘路径"由槽位反查所属池"使用：池链上的池地址互不相同，逐池做一次地址范围判断
    /// 即可定位。
    pub(crate) fn index_of_(&self, weak: NonNull<WeakChunk<()>>) -> Option<PoolIndex> {
        let slot_start = self as *const Self as usize + self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot_end = slot_start + self.capacity_ as usize * Self::SLOT_SIZE;
        let addr = weak.as_ptr() as usize;
        if addr < slot_start || addr >= slot_end {
            return Option::None;
        }
        let offset = addr - slot_start;
        if offset % Self::SLOT_SIZE != 0 {
            return Option::None;
        }
        Option::Some((offset / Self::SLOT_SIZE) as PoolIndex)
    }
}
