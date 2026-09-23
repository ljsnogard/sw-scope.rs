use core::{
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU32, AtomicPtr, Ordering},
};

use crate::{
    TrPreDrop,
    scope_tree_::PoolIndex,
    weak_::{PreDropRecord, WeakChunk},
};

/// StrongPool 中承载用户数据的一个块。
///
/// # 它为什么这么小
///
/// 决策「不复用内存空洞」之后，强块不再需要在池内维护任何出入记录：位置一旦分配就
/// 终身不变，整块池子最后一次性回收。于是状态、存活链链接、清理登记这些管理信息全部
/// 挂在 [`WeakChunk`]（对象身份槽位）上——但 `WeakChunk` 是**可选**的：对象可以直接以
/// `Owning` / `Sharing` 放进 `Scope`，只有调用 `retained()` 才补建它。强块自己只剩两样：
///
/// - 数据本身（`data_`）；
/// - 自己那份**强**引用计数与指回身份槽位的可选指针（[`StrongChunkHead`]）。
///
/// 强引用计数与弱槽位里的弱引用计数是**两个不同的东西**：前者数 "多少个 `Sharing` 共享
/// 这份数据"，后者数 "多少个 `Retain` 句柄指向这个对象"，因此各自保管、互不干扰。
#[repr(C)]
pub(crate) struct StrongChunk<T>
where
    T: ?Sized,
{
    chunk_head_: StrongChunkHead,
    data_: T,
}

/// 强块与类型无关的公共头部。
///
/// [`StrongChunkHead::weak_chunk_`] 指向本块的身份弱槽位：对象尚未 `retained()` 时为空，
/// 一旦补建便在强块的整个生命周期内保持不变。因此从强块出发可以找到存活链与清理登记所在
/// 的槽位；反向则不需要链接。
#[repr(C)]
pub(crate) struct StrongChunkHead {
    /// 强引用计数（仅 `Sharing<T>` 有效）。
    chunk_info_: StrongChunkHeadInfo,
    /// 指向本块的身份弱槽位；尚未建立身份时为 null。
    weak_chunk_: AtomicPtr<WeakChunk<()>>,
}


#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum StrongChunkState {
    /// 未分配资源
    Unused  = 0x00,
    /// 被 Owning 指针使用
    Owning  = 0x01,
    /// 被 Sharing 指针使用，此时强引用计数不为0
    Sharing = 0x02,
    /// 资源已被析构
    Desert  = 0x03,
}

impl StrongChunkHead {
    /// 构造空白头部
    pub(crate) const fn empty_(
        head: PoolIndex,
        next: PoolIndex,
    ) -> Self {
        StrongChunkHead {
            chunk_info_: StrongChunkHeadInfo::new(head, next),
            weak_chunk_: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// 本块的身份弱槽位；尚未 `retained()` 时为空。
    #[inline]
    pub(crate) fn weak_chunk(&self) -> Option<NonNull<WeakChunk<()>>> {
        let p = self.weak_chunk_.load(Ordering::Acquire);
        if p.is_null() {
            Option::None
        } else {
            Option::Some(unsafe { NonNull::new_unchecked(p)})
        }
    }

    /// 数据区的起始偏移（以 `StrongChunkHead` 计）。
    ///
    /// 注意返回的是 `size_of::<T>()` 为 0 时的偏移；`?Sized` 的 `T` 必须改用
    /// [`StrongChunk::data_offset`]，因为数据区的位置随 `T` 的元数据而变。
    #[inline]
    pub(crate) const fn base_size_() -> usize {
        mem::size_of::<StrongChunkHead>()
    }

    #[inline]
    pub(crate) fn chunk_state(&self) -> StrongChunkState {
        self.chunk_info_.chunk_state()
    }

    /// 当前强引用计数。
    #[inline]
    pub(crate) fn strong_count(&self) -> u32 {
        self.chunk_info_.strong_count()
    }

    /// 增加强引用计数，返回增加前的值
    #[inline]
    pub(crate) fn incr_strong_count(&self) -> u32 {
        self.chunk_info_.incr_strong_count()
    }

    /// 减少强引用计数，返回减少前的值。
    #[inline]
    pub(crate) fn decr_strong_count(&self) -> u32 {
        self.chunk_info_.decr_strong_count()
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> StrongChunk<T>
where
    T: ?Sized,
{
    /// 数据区起址。
    ///
    /// 用 `addr_of_mut!` 而不是手算偏移：既避免对齐错误，也保留 `?Sized` 的元数据。
    #[inline]
    pub(crate) fn data_ptr(&self) -> *mut T {
        let this = self as *const Self as *mut Self;
        // SAFETY: data_ 是 self 的第三个字段，按 repr(C) 布局紧跟在 base_ 之后
        unsafe { ptr::addr_of_mut!((*this).data_) }
    }

    /// 读出数据区引用。调用方必须自行保证数据仍然活着。
    ///
    /// # Safety
    ///
    /// 仅当 [`crate::weak_::DataState`] 表明数据尚未析构时才可调用。
    #[inline]
    pub(crate) unsafe fn data_ref_unchecked(&self) -> &T {
        // SAFETY: 由调用方保证数据仍然活着
        unsafe { &*self.data_ptr() }
    }

    #[inline]
    pub(crate) fn strong_count(&self) -> u32 {
        self.chunk_head_.strong_count()
    }

    /// 增加强引用计数（`Sharing::clone`）。
    #[inline]
    pub(crate) fn incr_strong_count(&self) -> u32 {
        self.chunk_head_.incr_strong_count()
    }

    /// 减少强引用计数并返回减少后的值（`Sharing::drop`）。
    #[inline]
    pub(crate) fn decr_strong_count(&self) -> u32 {
        self.chunk_head_.decr_strong_count()
    }

    /// 数据仍然活着时返回其引用。
    ///
    /// 数据区地址与 `?Sized` 的元数据都记在身份槽位上，因此这里经由它重建引用。
    pub(crate) fn try_get_data(&self) -> Option<&T> {
        if !self.is_data_alive() {
            Option::None
        } else {
            // SAFETY: 状态表明数据尚未析构；身份槽位比强块活得久
            unsafe { self.data_ptr().as_ref() }
        }
    }

    /// 数据仍然活着时返回其可变引用。
    pub(crate) fn try_get_data_mut(&mut self) -> Option<&mut T> {
        if !self.is_data_alive() {
            Option::None
        } else {
            // SAFETY: 状态表明数据尚未析构
            unsafe { self.data_ptr().as_mut() }
        }
    }

    /// 数据是否仍然活着。
    #[inline]
    pub(crate) fn is_data_alive(&self) -> bool {
        self.chunk_head_.chunk_state().is_data_alive()
    }

    /// 本块的身份弱槽位；对象从未 `retained()` 时为 [`None`]。
    ///
    /// 该指针一旦建立便不再改变：要么一直为空，要么在强块的整个生命周期内都指向同一个
    /// `WeakChunk`。
    #[inline]
    pub(crate) fn weak_chunk(&self) -> Option<NonNull<WeakChunk<()>>> {
        self.chunk_head_.weak_chunk()
    }

    /// 供**已建立身份弱槽位**的分配路径调用：绑定身份、清零强计数，并把数据登记进弱槽位的
    /// 清理登记。
    ///
    /// 直接以 `Owning` / `Sharing` 放进 `Scope` 的对象此时还没有弱槽位，不走本方法；它要到
    /// `retained()` 补建身份时才建立这份绑定。
    ///
    /// `record` 是调用方按"对象级 > 类型级 > 默认"选好的有效登记，交给类型无关的弱槽位保管。
    ///
    /// # Safety
    ///
    /// 调用者必须已经把 `T` 就地构造在 [`StrongChunk::data_ptr`] 处，且 `weak_chunk`
    /// 是本块的身份槽位。
    pub(crate) unsafe fn init_fresh_(
        &mut self,
        weak_chunk: NonNull<WeakChunk<()>>,
        data: *mut T,
        record: &'static PreDropRecord,
    ) {
        let meta_ = crate::weak_::meta_to_raw_(ptr::metadata(data as *const T));
        // SAFETY: 由调用方保证 weak_chunk 是身份槽位、数据已构造完毕
        unsafe { self.bind_fresh_(weak_chunk, meta_, record) };
    }

    /// 数据**已经就地构造完毕**，这里只做绑定：记录身份、清理登记与 `?Sized` 元数据。
    ///
    /// 与 [`StrongChunk::init_fresh_`] 的区别是元数据由调用方直接给出——`?Sized` 的
    /// emplace 路径只能这样拿到它（那时数据是刚被 `emplace` 写进去的）。
    ///
    /// # Safety
    ///
    /// `weak_chunk` 必须是本块的身份槽位；数据必须已经构造在数据区，且本块尚未登记过。
    pub(crate) unsafe fn bind_fresh_(
        &mut self,
        weak_chunk: NonNull<WeakChunk<()>>,
        meta_: *const (),
        record: &'static PreDropRecord,
    ) {
        self.chunk_head_ = StrongChunkHead::empty_(weak_chunk);
        let this = self as *const Self as *mut Self;
        // SAFETY: base_ 是首字段，addr_of_mut! 取到的是合法的薄指针
        let base = unsafe { ptr::addr_of_mut!((*this).chunk_head_) };
        // SAFETY: weak_chunk 是本对象的身份槽位，且此刻独占使用
        let weak = unsafe { &mut *(weak_chunk.as_ptr() as *mut WeakChunk<T>) };
        weak.set_strong_chunk(base);
        // 空操作登记（既不需要 Drop 也没有钩子）直接存 None，省掉一个无意义的指针
        let record_ = if record.is_noop_() {
            Option::None
        } else {
            Option::Some(record)
        };
        weak.set_record_erased_(record_, meta_);
    }
}

impl<T> StrongChunk<T>
where
    T: Sized,
{
    /// 就地把 `value` 构造进数据区，并用调用方指定的有效登记完成绑定。
    ///
    /// # Safety
    ///
    /// `weak_chunk` 必须是本块的身份槽位，且本块尚未登记过任何数据。
    pub(crate) unsafe fn init_with_record_(
        &mut self,
        weak_chunk: NonNull<WeakChunk<()>>,
        value: T,
        record: &'static PreDropRecord,
    ) {
        let data = self.data_ptr();
        // SAFETY: 数据区已分配、对齐且尚未初始化；由调用方保证独占
        unsafe { data.write(value) };
        // SAFETY: 由调用方保证 weak_chunk 是身份槽位
        unsafe { self.init_fresh_(weak_chunk, data, record) };
    }

    /// 就地把 `value` 构造进数据区并完成登记（无钩子的默认登记）。
    ///
    /// 分配路径的真实流程是"先备好壳、再由 `TrEmplace` 就地构造"，本方法把该流程压成
    /// 一步，供那些不需要就地构造的简单类型使用。
    ///
    /// # Safety
    ///
    /// `weak_chunk` 必须是本块的身份槽位，且本块尚未登记过任何数据。
    pub(crate) unsafe fn init_with_(
        &mut self,
        weak_chunk: NonNull<WeakChunk<()>>,
        value: T,
    ) {
        // SAFETY: 由调用方保证 weak_chunk 是身份槽位
        unsafe { self.init_with_record_(weak_chunk, value, PreDropRecord::of::<T>()) };
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

struct StrongChunkHeadInfo {
    /// 距离 StrongPool 有多少个 cell
    head_offset_: PoolIndex,
    /// 距离下一个 StrongChunk 有多少个 cell
    next_offset_: PoolIndex,
    /// StrongChunk 的状态，包含 StrongChunkState （高2位）和强引用计数
    atomic_flag_: AtomicU32,
}

impl StrongChunkHeadInfo {
    const K_REFC_SHIFT: u32 = 28;
    const K_MUTEX_MASK: u32 =
        (StrongChunkState::K_U8_MASK as u32) << Self::K_REFC_SHIFT;

    const K_STATE_MASK: u32 = 0x03 << Self::K_REFC_SHIFT;
    const K_REFC_MASK: u32 = (1 << Self::K_REFC_SHIFT) - 1;

    pub const fn new(
        head_offset: PoolIndex,
        next_offset: PoolIndex,
    ) -> Self {
        StrongChunkHeadInfo {
            head_offset_: head_offset,
            next_offset_: next_offset,
            atomic_flag_: AtomicU32::new(0),
        }
    }

    fn head_offset(&self) -> PoolIndex {
        self.head_offset_
    }

    fn next_offset(&self) -> PoolIndex {
        self.next_offset_
    }

    fn chunk_state(&self) -> StrongChunkState {
        let v = self.atomic_flag_.load(Ordering::Acquire);
        StrongChunkState::new((v >> Self::K_REFC_SHIFT) as u8)
    }

    /// 当前强引用计数。
    #[inline]
    fn strong_count(&self) -> u32 {
        self.atomic_flag_.load(Ordering::Acquire)
    }

    /// 增加强引用计数，返回增加前的值
    #[inline]
    fn incr_strong_count(&self) -> u32 {
        self.atomic_flag_.fetch_add(1, Ordering::AcqRel)
    }

    /// 减少强引用计数，返回减少前的值。
    #[inline]
    fn decr_strong_count(&self) -> u32 {
        self.atomic_flag_.fetch_sub(1, Ordering::AcqRel)
    }
}


impl StrongChunkState {
    const K_U8_MASK: u8 = 0x03;

    pub const fn new(v: u8) -> Self {
        let v = Self::K_U8_MASK & v;
        match v {
            0x00 => StrongChunkState::Unused,
            0x01 => StrongChunkState::Owning,
            0x02 => StrongChunkState::Sharing,
            0x03 => StrongChunkState::Desert,
            _ => unreachable!(),
        }
    }

    pub const fn is_data_alive(&self) -> bool {
        match self {
            StrongChunkState::Owning | StrongChunkState::Sharing => true,
            _ => false
        }
    }
}
