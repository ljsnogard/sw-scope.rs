use core::{
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU32, Ordering},
};

use crate::{
    strong_::PreDropRecord,
    weak_::{data_ptr_of_, WeakSlot},
};

/// Arena 中承载用户数据的一个块。
///
/// # 它为什么这么小
///
/// 决策「不复用内存空洞」之后，强块不再需要在池内维护任何出入记录：位置一旦分配就
/// 终身不变，整块池子最后一次性回收。
///
/// 强块头部承担两件事：
///
/// - 自己那份**强**引用计数与指回身份的指针；
/// - 类型擦除的 `PreDrop` 登记与 `?Sized` 元数据（弱槽位不再保存它们）。
///
/// 存活链不逐槽位保存，而是由弱池链与槽位状态在需要时反推。
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
/// 强块通过 [`StrongChunkBase::weak_chunk`] 单向指回身份槽位；清理登记与元数据就在
/// 强块头部，因此清盘一侧只需要强块指针就能完成类型擦除析构。
#[repr(C)]
pub(crate) struct StrongChunkBase {
    /// 强引用计数（仅 `Sharing<T>` 有效）。
    refc_: AtomicU32,

    /// 指回本块的身份弱槽位。
    weak_chunk_: NonNull<WeakSlot<()>>,

    /// 类型擦除的清理登记；`None` 表示既不需要 `Drop` 也没有 `PreDrop` 钩子。
    ///
    /// 新设计把 `PreDrop` 与 `?Sized` 元数据从弱槽位移到这里：弱槽位只承担身份、
    /// 状态与空闲链，强块头部才是"这块数据怎么析构"的权威。
    predrop_rec_: Option<&'static PreDropRecord>,

    /// 数据区的 `?Sized` 元数据（切片长度 / 虚表）；`Sized` 时为空指针。
    ///
    /// 它必须和 `predrop_rec_` 一起保存：清理入口按 `T` 单态化后需要它才能重建
    /// `*mut T`。
    meta_: *const (),
}


impl StrongChunkBase {
    /// 构造空白头部。`weak_chunk` 必须由分配路径立刻补上。
    pub(crate) const fn empty_(weak_chunk: NonNull<WeakSlot<()>>) -> Self {
        StrongChunkBase {
            refc_: AtomicU32::new(0),
            weak_chunk_: weak_chunk,
            predrop_rec_: Option::None,
            meta_: ptr::null(),
        }
    }

    /// 本块的身份弱槽位。
    #[inline]
    pub(crate) const fn weak_chunk(&self) -> NonNull<WeakSlot<()>> {
        self.weak_chunk_
    }

    /// 数据区的 `?Sized` 元数据。
    #[inline]
    pub(crate) const fn meta_(&self) -> *const () {
        self.meta_
    }

    /// 类型擦除的清理登记。
    #[inline]
    pub(crate) const fn predrop_rec_(&self) -> Option<&'static PreDropRecord> {
        self.predrop_rec_
    }

    /// 绑定清理登记与 `?Sized` 元数据。只在数据刚构造完成时调用。
    #[inline]
    pub(crate) fn set_record_meta_(
        &mut self,
        record: Option<&'static PreDropRecord>,
        meta: *const (),
    ) {
        self.predrop_rec_ = record;
        self.meta_ = meta;
    }

    /// 按登记执行清理：先跑 `PreDrop` 钩子，再析构数据。
    ///
    /// # Safety
    ///
    /// 必须先由状态机认领销毁，且恰好调用一次。
    pub(crate) unsafe fn drop_data_(&self) {
        let Option::Some(record) = self.predrop_rec_ else {
            return;
        };
        // SAFETY: 由调用方保证已认领且尚未析构；登记与元数据来自同一块数据
        unsafe { record.run_(self as *const Self as *mut u8, self.meta_) };
    }

    /// 按 `U` 重建数据引用；调用方必须保证数据仍然活着。
    ///
    /// # Safety
    ///
    /// 数据必须尚未析构，且 `U` 必须与当初分配的 `T` 是同一个类型。
    pub(crate) unsafe fn data_ref_as<U: ?Sized + ptr::Pointee>(&self) -> &U {
        // SAFETY: 强块首址与 StrongChunk<U> 首址相同；元数据来自同一次分配
        let data = unsafe { data_ptr_of_::<U>(self as *const Self as *mut u8, self.meta_) };
        // SAFETY: 由调用方保证数据尚未析构
        unsafe { &*data }
    }

    /// 按 `U` 重建数据可变引用；调用方必须保证数据仍然活着且独占。
    ///
    /// # Safety
    ///
    /// 同 [`StrongChunkBase::data_ref_as`]，且调用方必须保证独占访问。
    pub(crate) unsafe fn data_ref_mut_as<U: ?Sized + ptr::Pointee>(&mut self) -> &mut U {
        // SAFETY: 同 data_ref_as
        let data = unsafe { data_ptr_of_::<U>(self as *mut Self as *mut u8, self.meta_) };
        // SAFETY: 由调用方保证数据尚未析构且独占
        unsafe { &mut *data }
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

    /// 增加强引用计数（`Sharing::clone`）。
    #[inline]
    pub(crate) fn incr_strong_count(&self) -> u32 {
        self.base_.incr_strong_count()
    }

    /// 减少强引用计数并返回减少后的值（`Sharing::drop`）。
    #[inline]
    pub(crate) fn decr_strong_count(&self) -> u32 {
        self.base_.decr_strong_count()
    }

    /// 数据仍然活着时返回其引用。
    ///
    /// 数据区地址与 `?Sized` 的元数据都记在身份槽位上，因此这里经由它重建引用。
    pub(crate) fn try_get_data(&self) -> Option<&T> {
        if !self.is_data_alive() {
            return Option::None;
        }
        // SAFETY: 状态表明数据尚未析构；登记与元数据就在强块头部
        Option::Some(unsafe { self.base_.data_ref_as::<T>() })
    }

    /// 数据仍然活着时返回其可变引用。
    pub(crate) fn try_get_data_mut(&mut self) -> Option<&mut T> {
        if !self.is_data_alive() {
            return Option::None;
        }
        // SAFETY: 状态表明数据尚未析构，且 &mut self 保证独占访问
        Option::Some(unsafe { self.base_.data_ref_mut_as::<T>() })
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
    pub(crate) fn weak_chunk(&self) -> NonNull<WeakSlot<()>> {
        self.base_.weak_chunk()
    }

    /// 本块的清理登记（保存在强块头部）。
    #[inline]
    pub(crate) fn predrop_rec_(&self) -> Option<&'static PreDropRecord> {
        self.base_.predrop_rec_()
    }

    /// 供分配路径调用：绑定身份、清零强计数，并把清理登记与元数据写进强块头部。
    ///
    /// `record` 是调用方按"对象级 > 类型级 > 默认"选好的有效登记，交给类型无关的弱槽位保管。
    ///
    /// # Safety
    ///
    /// 调用者必须已经把 `T` 就地构造在 [`StrongChunk::data_ptr`] 处，且 `weak_chunk`
    /// 是本块的身份槽位。
    pub(crate) unsafe fn init_fresh_(
        &mut self,
        weak_chunk: NonNull<WeakSlot<()>>,
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
        weak_chunk: NonNull<WeakSlot<()>>,
        meta_: *const (),
        record: &'static PreDropRecord,
    ) {
        self.base_ = StrongChunkBase::empty_(weak_chunk);
        let this = self as *const Self as *mut Self;
        // SAFETY: base_ 是首字段，addr_of_mut! 取到的是合法的薄指针
        let base = unsafe { ptr::addr_of_mut!((*this).base_) };
        // SAFETY: weak_chunk 是本对象的身份槽位，且此刻独占使用
        let weak = unsafe { &mut *(weak_chunk.as_ptr() as *mut WeakSlot<T>) };
        weak.set_strong_chunk(base);
        // 空操作登记（既不需要 Drop 也没有钩子）直接存 None，省掉一个无意义的指针
        let record_ = if record.is_noop_() {
            Option::None
        } else {
            Option::Some(record)
        };
        self.base_.set_record_meta_(record_, meta_);
    }
}

impl<T> StrongChunk<T> {
    /// 就地把 `value` 构造进数据区，并用调用方指定的有效登记完成绑定。
    ///
    /// # Safety
    ///
    /// `weak_chunk` 必须是本块的身份槽位，且本块尚未登记过任何数据。
    pub(crate) unsafe fn init_with_record_(
        &mut self,
        weak_chunk: NonNull<WeakSlot<()>>,
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
        weak_chunk: NonNull<WeakSlot<()>>,
        value: T,
    ) {
        // SAFETY: 由调用方保证 weak_chunk 是身份槽位
        unsafe { self.init_with_record_(weak_chunk, value, PreDropRecord::of::<T>()) };
    }

}
