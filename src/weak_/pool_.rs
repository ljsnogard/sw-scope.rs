use core::{
    alloc::{AllocError, Allocator, Layout},
    mem,
    ptr::NonNull,
};

use crate::{
    scope_inner_::{PoolIndex, ScopeInner},
    weak_::WeakSlot,
};

/// 一个专门用于存储 `Retain<T>` 所需弱引用槽位（slot）的内存池。
///
/// # 布局
///
/// 与 [`crate::strong_::StrongPool`] 一样是内嵌式的：池头（即 `Self`）位于块首，槽位数组
/// 紧随其后，并记录在 `cell_offset_` 中。因此由任意槽位地址减去 `cell_offset_` 与池头
/// 大小，即可反推出所属池的地址；`WeakChunkState::pool_order_` 正是为此保留的槽位序号。
/// `cell_offset_` 记录的是槽位数组相对池头的**槽位个数**（向上取整），因此槽位数组的
/// 起点始终按 [`WeakSlot<()>`] 对齐；它不假设池头大小恰好是槽位大小的整数倍。
///
/// 池中槽位一律按 [`WeakSlot<()>`] 解释，`WeakSlot<T>` 之于任意 `T` 只在类型上区分，
/// 尺寸恒等于 `WeakSlot<()>`，正是靠这一点池才能在不为每种 `T` 单独布局的前提下复用
/// 槽位。又由于索引一律是 [`PoolIndex`]（u16），池的槽位数天然被限制在
/// `PoolIndex::MAX` 以内。
///
/// 本池所属的 [`ScopeInner`] 由池自己记录（[`WeakPool::set_scope`]）；池只负责把它存下来
/// 再原样交还。弱池随域持有，不再由 root 共享。
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
/// - 当 `alloc_count_ == capacity_` 时链表已空，此时 `latest_free_` 的取值不再有意义，
///   也无须维护。
///
/// # 存活链为什么不需要逐槽位指针
///
/// 分配总是从空闲链头摘取：**从未使用的槽位按全局下标递增分配**，而归还的槽位会被
/// 优先复用。于是"按全局下标顺序扫描、跳过空闲槽位"就得到一条相对有序的构造顺序；
/// 被复用的槽位会回到它原来的下标位置，所以是"相对"而非严格时间顺序。
///
/// 反推从链尾池的最后一个空闲槽位开始，走到池尾后回绕到链头，最终回到起点。这样
/// 不需要在每个槽位里维护 `prev_live_` / `next_live_`，只需要读槽位状态即可。完整
/// 算法见 [`WeakPool::for_each_live_from_`]。
///
/// # 槽位何时归还
///
/// 归还条件不是"弱引用计数归零"这么单一，而要综合槽位状态判断：只有"没有任何 `Retain`"
/// 且"数据已经析构（或从未需要析构）"时，槽位才会回到空闲链。判断依据见
/// [`crate::weak_::DataState`]。
#[repr(C)]
pub(crate) struct WeakPool {
    // 最多可以存储多少个 WeakChunk，也就是紧随池头之后的槽位数组长度
    //
    // 旧实现用 `AtomicFlags<u16>` 的最高位兼作池锁；新设计先把池锁去掉，
    // 等弱池的同步方案确定后再恢复。原字段见文件末尾注释。
    capacity_: PoolIndex,
    // 已经分配出去的 WeakChunk 数量，注意 WeakPool 的分配顺序是不可知的
    alloc_count_: PoolIndex,
    // 槽位数组相对池头的偏移量（按 WeakChunk 个数计算，而非字节）
    cell_offset_: PoolIndex,
    // 空闲槽位虚拟链表的表头；仅当 used_length_ < capacity_ 时有效，
    // used_length_ == capacity_ 时链表已空，该字段不再有意义
    latest_free_: PoolIndex,

    // 同一条内存链上的兄弟池，供池容量不足时串联扩展使用
    prev_: Option<NonNull<WeakPool>>,
    next_: Option<NonNull<WeakPool>>,
    // 本池所属的 ScopeInner：弱池随域持有，不再由 root 共享
    scope_: Option<NonNull<ScopeInner>>,
}

// 旧字段定义（池头自带原子锁、所属域叫 `scope_`）：
//
// pub(crate) struct WeakPool {
//     capacity_: AtomicFlags<u16>,
//     alloc_count_: PoolIndex,
//     cell_offset_: PoolIndex,
//     latest_free_: PoolIndex,
//     prev_: Option<NonNull<WeakPool>>,
//     next_: Option<NonNull<WeakPool>>,
//     scope_: Option<NonNull<ScopeInner>>,
// }

impl WeakPool {
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
        alloc: &dyn Allocator,
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
        alloc: &dyn Allocator,
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
        self.alloc_count_
    }

    /// 池当前的空闲槽位数量。
    pub const fn free_count(&self) -> PoolIndex {
        self.capacity_ - self.alloc_count_
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
    pub const fn next(&self) -> Option<NonNull<WeakPool>> {
        self.next_
    }

    /// 池链上的上一个池。
    pub const fn prev(&self) -> Option<NonNull<WeakPool>> {
        self.prev_
    }

    /// 本池所属的 ScopeInner。
    pub const fn root(&self) -> Option<NonNull<ScopeInner>> {
        self.scope_
    }

    /// 绑定本池所属的 [`ScopeInner`]。
    pub fn set_scope(&mut self, scope: NonNull<ScopeInner>) {
        self.scope_ = Option::Some(scope);
    }

    /// 把 `next` 接到本池之后，并回填其反向指针，从而把两个池串成双向链的一环。
    pub fn link_siblings(&mut self, next: &mut WeakPool) {
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
        if self.alloc_count_ == self.capacity_ {
            return Option::None;
        }
        let index = self.latest_free_;
        // SAFETY: 池未满时 index 必定落在 0..capacity_ 内，见上方关于链表的说明
        let slot = unsafe { self.slot_mut_at_(index) };
        self.latest_free_ = slot.next_freed();
        self.alloc_count_ += 1;
        Option::Some(index)
    }

    /// 把一个槽位归还到空闲链表的表头。
    ///
    /// 归还只重置槽位的空闲链接、状态、计数与析构登记；"数据是否已经析构"由调用方在
    /// 归还之前判断并完成。当 `index` 不是本池当前已分配的槽位时返回 `false`，池的状态
    /// 不变。
    pub fn deallocate(&mut self, index: PoolIndex) -> bool {
        if index >= self.capacity_ || self.alloc_count_ == 0 {
            return false;
        }
        let head = self.latest_free_;
        // SAFETY: index 已按 capacity_ 校验，落在槽位数组范围内
        let slot = unsafe { self.slot_mut_at_(index) };
        slot.link_as_freed(head);
        self.latest_free_ = index;
        self.alloc_count_ -= 1;
        true
    }

    /// 读取某个槽位记录的所属池序号。
    pub fn pool_order_of(&self, index: PoolIndex) -> PoolIndex {
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        let slot = unsafe { self.slot_at_(index) };
        slot.pool_order()
    }

    /// 列出全部槽位，其中可能包含尚未使用的。
    pub fn slots(&mut self) -> NonNull<[WeakSlot<()>]> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let chunks = unsafe { this.byte_add(offset) as *mut WeakSlot<()> };
        let ptr = core::ptr::slice_from_raw_parts_mut(chunks, self.capacity_ as usize);
        unsafe { NonNull::new_unchecked(ptr) }
    }
    /// 一个槽位的大小，池中所有槽位一律按 `WeakChunk<()>` 解释
    const SLOT_SIZE: usize = mem::size_of::<WeakSlot<()>>();

    /// 池头占据的槽位数：向上取整，保证槽位数组起点仍满足 `WeakChunk<()>` 的对齐要求。
    ///
    /// 池头随 `Root` 等参数变大时，这里会自然跟着变，不再依赖"池头恰好等于若干个槽位"
    /// 这种脆弱假设。
    const HEADER_SLOT_COUNT: usize = mem::size_of::<Self>().div_ceil(Self::SLOT_SIZE);

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
    pub(crate) fn of_slot_(weak: NonNull<WeakSlot<()>>) -> Option<NonNull<Self>> {
        let addr = weak.as_ptr() as usize;
        // 先按槽位对齐校验，避免对错位地址解引用去读 `pool_order_`
        if !addr.is_multiple_of(mem::align_of::<WeakSlot<()>>()) {
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
        let size =
            Self::HEADER_SLOT_COUNT * Self::SLOT_SIZE + (cell_count as usize) * Self::SLOT_SIZE;
        let align = mem::align_of::<Self>().max(mem::align_of::<WeakSlot<()>>());
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
            unsafe { (base as *mut u8).byte_add(header_bytes) } as *mut WeakSlot<()>,
            cell_count as usize,
        );
        // 池头与槽位数组都在同一块刚刚分配、尚未共享出去的内存上，因此独占地写入是安全的。
        unsafe {
            let pool = &mut *base;
            let slots = &mut *all_slots;
            WeakSlot::<()>::init_slots(slots, cell_count);
            pool.capacity_ = cell_count;
            pool.alloc_count_ = 0;
            pool.cell_offset_ = Self::HEADER_SLOT_COUNT as PoolIndex;
            pool.latest_free_ = 0;
            pool.prev_ = Option::None;
            pool.next_ = Option::None;
            pool.scope_ = Option::None;
        }
        // SAFETY: base 来自分配器返回的非空内存块，必然非空
        unsafe { NonNull::new_unchecked(base) }
    }

    /// 按槽位序号取只读槽位引用。
    ///
    /// # Safety
    ///
    /// `index` 必须小于 `capacity_`，否则返回的引用会越出槽位数组。
    pub(crate) unsafe fn slot_at_(&self, index: PoolIndex) -> &WeakSlot<()> {
        let this = self as *const Self as *const u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot = unsafe {
            this.byte_add(offset + index as usize * Self::SLOT_SIZE) as *const WeakSlot<()>
        };
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        unsafe { &*slot }
    }

    /// 按槽位序号取可写槽位引用。
    ///
    /// # Safety
    ///
    /// `index` 必须小于 `capacity_`，否则返回的引用会越出槽位数组。
    unsafe fn slot_mut_at_(&mut self, index: PoolIndex) -> &mut WeakSlot<()> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot = unsafe {
            this.byte_add(offset + index as usize * Self::SLOT_SIZE) as *mut WeakSlot<()>
        };
        // SAFETY: 由调用方保证 index 小于 capacity_，指向的是池内一个对齐的槽位
        unsafe { &mut *slot }
    }

    /// 判断某个槽位是否落在本池的槽位数组内；是则返回它在池内的序号。
    ///
    /// 供清盘路径"由槽位反查所属池"使用：池链上的池地址互不相同，逐池做一次地址范围判断
    /// 即可定位。
    pub(crate) fn index_of_(&self, weak: NonNull<WeakSlot<()>>) -> Option<PoolIndex> {
        let slot_start =
            self as *const Self as usize + self.cell_offset_ as usize * Self::SLOT_SIZE;
        let slot_end = slot_start + self.capacity_ as usize * Self::SLOT_SIZE;
        let addr = weak.as_ptr() as usize;
        if addr < slot_start || addr >= slot_end {
            return Option::None;
        }
        let offset = addr - slot_start;
        if !offset.is_multiple_of(Self::SLOT_SIZE) {
            return Option::None;
        }
        Option::Some((offset / Self::SLOT_SIZE) as PoolIndex)
    }
    /// 找到本池中**下标最大**的空闲槽位。
    ///
    /// 空闲的判定是状态字为 [`DataState::Reclaimed`](crate::weak_::DataState::Reclaimed)：
    /// 从未分配过的槽位与归还回来的槽位都满足它。
    pub(crate) fn last_free_index_(&self) -> Option<PoolIndex> {
        let mut index = self.capacity_;
        while index > 0 {
            index -= 1;
            // SAFETY: index 从 capacity_-1 递减到 0，始终落在槽位数组内
            let slot = unsafe { self.slot_at_(index) };
            if slot.is_free_() {
                return Option::Some(index);
            }
        }
        Option::None
    }

    /// 从任意池出发，按"全局槽位顺序"遍历整条池链上的所有存活槽位。
    ///
    /// 全局槽位顺序是：从链头池到链尾池，每个池内部从下标 0 到 `capacity_-1`。
    /// 遍历起点是**链尾池的最后一个空闲槽位**；从这个位置向前走，走到链尾池末尾后
    /// 回绕到链头池，直到走回起点为止。
    ///
    /// # 为什么不需要槽位里的存活链指针
    ///
    /// 弱池的分配是严格的"逐个 Slot"推进：
    ///
    /// - 新池的空闲链是 `0 -> 1 -> ... -> capacity-1 -> capacity`，分配从链头摘取，
    ///   所以**从未使用过的槽位按全局下标递增的顺序被分配**；
    /// - 某个槽位被归还时会被放到空闲链头，下一次分配**优先**复用它，而不是先推进到
    ///   更大的下标。
    ///
    /// 因此，除"被复用的归还槽位"之外，一个槽位的全局下标顺序就是它的构造顺序；空闲
    /// 槽位则是这条顺序上的空洞。只要把所有槽位按全局下标顺序扫一遍、跳过空闲槽位，
    /// 就得到一条**相对有序的构造顺序**的存活链。被复用的槽位会回到它原来的下标位置，
    /// 因此这里得到的是"相对"而不是严格的分配时间顺序——这正是不需要逐槽位存活链
    /// 指针所换来的代价。
    ///
    /// 起点选在链尾池的最后一个空闲槽位，是为了让遍历从"最新一段已经分配过的范围"
    /// 开始，前方紧接的空闲尾巴与链头回绕部分都能被同一条循环覆盖；如果链尾池已满，
    /// 则从它的下标 0 开始。整个过程只读槽位状态，不需要任何逐槽位的 `prev`/`next`。
    ///
    /// # 复杂度
    ///
    /// 时间与所有池的槽位总数成正比，额外空间为 O(1)（只维护池游标与下标）。
    pub(crate) fn for_each_live_from_(
        any_pool: NonNull<WeakPool>,
        visit: &mut impl FnMut(NonNull<WeakSlot<()>>),
    ) {
        // 先找到链尾：存活链的起点只看链尾池。
        let mut tail = any_pool;
        loop {
            // SAFETY: 调用方保证 any_pool 指向有效池；链上的池都由同一分配器持有
            let next = unsafe { tail.as_ref() }.next();
            match next {
                Option::Some(next) => tail = next,
                Option::None => break,
            }
        }

        // 再找到链头，供回绕使用。
        let mut head = tail;
        loop {
            // SAFETY: 同上
            let prev = unsafe { head.as_ref() }.prev();
            match prev {
                Option::Some(prev) => head = prev,
                Option::None => break,
            }
        }

        // 起点：链尾池的最后一个空闲槽位；池满则从 0 开始。
        // SAFETY: tail 有效
        let start = unsafe { tail.as_ref() }
            .last_free_index_()
            .map_or(0, |index| index as usize);
        // SAFETY: tail 有效；start < capacity_
        let tail_capacity = unsafe { tail.as_ref() }.capacity_ as usize;

        // 1) 链尾池：start..capacity
        unsafe { tail.as_ref() }.visit_live_range_(start, tail_capacity, visit);

        // 2) 链头到链尾之前的池：整个池
        let mut cursor = head;
        while cursor != tail {
            // SAFETY: cursor 是链上的有效池
            let pool = unsafe { cursor.as_ref() };
            pool.visit_live_range_(0, pool.capacity_ as usize, visit);
            match pool.next() {
                Option::Some(next) => cursor = next,
                // 链应当在到达 tail 前一直有 next；防御性回绕，避免死循环
                Option::None => break,
            }
        }

        // 3) 链尾池：0..start，补完回绕部分
        if start > 0 {
            // SAFETY: tail 有效
            unsafe { tail.as_ref() }.visit_live_range_(0, start, visit);
        }
    }

    /// 访问本池 `[start, end)` 内所有存活槽位。
    ///
    /// 空闲槽位（状态为 [`DataState::Reclaimed`](crate::weak_::DataState::Reclaimed)）
    /// 是构造顺序上的空洞，直接跳过。
    fn visit_live_range_(
        &self,
        start: usize,
        end: usize,
        visit: &mut impl FnMut(NonNull<WeakSlot<()>>),
    ) {
        debug_assert!(start <= end);
        debug_assert!(end <= self.capacity_ as usize);
        for index in start..end {
            // SAFETY: index 落在 [0, capacity_) 内
            let slot = unsafe { self.slot_at_(index as PoolIndex) };
            if !slot.is_free_() {
                visit(NonNull::from(slot).cast());
            }
        }
    }
}
