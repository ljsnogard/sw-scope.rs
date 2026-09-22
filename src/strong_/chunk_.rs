use core::{
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::weak_::WeakChunk;

/// Arena 中承载用户数据的一个块。
///
/// # 它为什么这么小
///
/// 决策「不复用内存空洞」之后，强块不再需要在池内维护任何出入记录：位置一旦分配就
/// 终身不变，整块池子最后一次性回收。于是状态、存活链链接、析构登记这些管理信息全部
/// 移到了 [`WeakChunk`]（对象的身份所在，也是生命周期最长的一环），强块自己只剩两样：
///
/// - 数据本身（`data_`）；
/// - 自己那份**强**引用计数与指回身份的指针（[`StrongChunkBase`]）。
///
/// 强引用计数与弱槽位里的弱引用计数是**两个不同的东西**：前者数 "多少个 `Sharing` 共享
/// 这份数据"，后者数 "多少个 `Retain` 句柄指向这个对象"，因此各自保管、互不干扰。
#[repr(C)]
pub(crate) struct StrongChunk<T>
where
    T: ?Sized,
{
    base_: StrongChunkBase,
    data_: T,
}

/// 强块与类型无关的公共头部。
///
/// 弱槽位通过 [`StrongChunkBase::weak_chunk`] 单向指回身份，因此从强块出发可以找到
/// 存活链与析构登记所在的槽位；反向则不需要链接。
#[repr(C)]
pub(crate) struct StrongChunkBase {
    /// 强引用计数（仅 `Sharing<T>` 有效）。
    refc_: AtomicU32,
    /// 指回本块的身份弱槽位。
    weak_chunk_: NonNull<WeakChunk<()>>,
}

impl StrongChunkBase {
    /// 构造空白头部。`weak_chunk` 必须由分配路径立刻补上。
    pub(crate) const fn empty_(weak_chunk: NonNull<WeakChunk<()>>) -> Self {
        StrongChunkBase {
            refc_: AtomicU32::new(0),
            weak_chunk_: weak_chunk,
        }
    }

    /// 本块的身份弱槽位。
    #[inline]
    pub(crate) const fn weak_chunk(&self) -> NonNull<WeakChunk<()>> {
        self.weak_chunk_
    }

    /// 数据区的起始偏移（以 `StrongChunkBase` 计）。
    ///
    /// 注意返回的是 `size_of::<T>()` 为 0 时的偏移；`?Sized` 的 `T` 必须改用
    /// [`StrongChunk::data_offset`]，因为数据区的位置随 `T` 的元数据而变。
    #[inline]
    pub(crate) const fn base_size_() -> usize {
        mem::size_of::<StrongChunkBase>()
    }

    /// 当前强引用计数。
    #[inline]
    pub(crate) fn strong_count(&self) -> u32 {
        self.refc_.load(Ordering::Acquire)
    }

    /// 增加强引用计数。
    #[inline]
    pub(crate) fn incr_strong_count(&self) -> u32 {
        self.refc_.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// 减少强引用计数，返回减少后的值。
    #[inline]
    pub(crate) fn decr_strong_count(&self) -> u32 {
        self.refc_.fetch_sub(1, Ordering::AcqRel) - 1
    }

    /// 把强计数置为 0，供归还/复用时收口。
    #[inline]
    pub(crate) fn reset_strong_count(&self) {
        self.refc_.store(0, Ordering::Release);
    }
}

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
        // SAFETY: data_ 是 self 的第二个字段，按 repr(C) 布局紧跟在 base_ 之后
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
        self.base_.strong_count()
    }

    /// 数据仍然活着时返回其引用。
    ///
    /// 数据区地址与 `?Sized` 的元数据都记在身份槽位上，因此这里经由它重建引用。
    pub(crate) fn try_get_data(&self) -> Option<&T> {
        if !self.is_data_alive() {
            return Option::None;
        }
        // SAFETY: 状态表明数据尚未析构；身份槽位比强块活得久
        let weak = unsafe { self.base_.weak_chunk().as_ref() };
        // SAFETY: 状态表明数据尚未析构
        Option::Some(unsafe { weak.data_ref_as::<T>() })
    }

    /// 数据仍然活着时返回其可变引用。
    pub(crate) fn try_get_data_mut(&mut self) -> Option<&mut T> {
        if !self.is_data_alive() {
            return Option::None;
        }
        // SAFETY: 状态表明数据尚未析构；身份槽位比强块活得久，且 &mut self 保证独占访问
        let weak = unsafe { self.base_.weak_chunk().as_mut() };
        // SAFETY: 状态表明数据尚未析构
        Option::Some(unsafe { weak.data_ref_mut_as::<T>() })
    }

    /// 数据是否仍然活着。状态权威在弱槽位，这里顺着身份指针去读。
    #[inline]
    pub(crate) fn is_data_alive(&self) -> bool {
        // SAFETY: 弱槽位必然比强块活得久，见 WeakChunk 的文档
        let weak = unsafe { self.base_.weak_chunk().as_ref() };
        weak.data_state().is_data_alive()
    }

    /// 本块的身份弱槽位。
    #[inline]
    pub(crate) fn weak_chunk(&self) -> NonNull<WeakChunk<()>> {
        self.base_.weak_chunk()
    }

    /// 供分配路径调用：绑定身份、清零强计数，并把数据登记进弱槽位的析构表。
    ///
    /// # Safety
    ///
    /// 调用者必须已经把 `T` 就地构造在 [`StrongChunk::data_ptr`] 处，且 `weak_chunk`
    /// 是本块的身份槽位。
    pub(crate) unsafe fn init_fresh_(
        &mut self,
        weak_chunk: NonNull<WeakChunk<()>>,
        data: *mut T,
    ) {
        self.base_ = StrongChunkBase::empty_(weak_chunk);
        let this = self as *const Self as *mut Self;
        // SAFETY: base_ 是首字段，addr_of_mut! 取到的是合法的薄指针
        let base = unsafe { ptr::addr_of_mut!((*this).base_) };
        // SAFETY: weak_chunk 是本对象的身份槽位，且此刻独占使用
        let weak = unsafe { &mut *(weak_chunk.as_ptr() as *mut WeakChunk<T>) };
        weak.set_strong_chunk(base);
        // 在这里（有具体 T 的地方）取到"每类型一份"的清理登记，再交给类型无关的弱槽位保管
        let record_ = if mem::needs_drop::<T>() {
            Option::Some(crate::weak_::PreDropRecord::of::<T>())
        } else {
            Option::None
        };
        let meta_ = crate::weak_::meta_to_raw_(ptr::metadata(data as *const T));
        weak.set_record_erased_(record_, meta_);
    }
}

impl<T> StrongChunk<T> {
    /// 就地把 `value` 构造进数据区并完成登记。
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
        let data = self.data_ptr();
        // SAFETY: 数据区已分配、对齐且尚未初始化；由调用方保证独占
        unsafe { data.write(value) };
        // SAFETY: 由调用方保证 weak_chunk 是身份槽位
        unsafe { self.init_fresh_(weak_chunk, data) };
    }

}
