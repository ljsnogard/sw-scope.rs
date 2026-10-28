//! 强块（strong chunk）的头部、可选 meta 字、可选 Predrop 对象与数据区。
//!
//! # 新设计
//!
//! 强块的实际内存布局是：
//!
//! ```text
//! (StrongChunkBase, [meta 字], [predrop 对象], data)
//! ```
//!
//! 方括号表示可选部分。这里**不能**用一个固定的 Rust struct 字段顺序来表达真实布局：
//!
//! - 类型级 vtable 由 `RootScope` 的 `metas_` B+ 树按 `ConstTypeId` 保存；强块只保存
//!   这个 id 与自己的**每对象** `?Sized` 元数据字；
//! - `T: Sized` 时不需要 meta 槽；`T: ?Sized` 时槽里直接存 `T::Metadata` 的裸字；
//! - `Predrop` 是 `P: TrPredrop<T>` 对象，直接就地存储，且与 `T` 是否 Sized 无关；
//! - 实际数据位置由 `dat_offset_` 给出。
//!
//! 因此布局只能由 [`StrongChunk<T, P>`] 的**工厂方法**决定：
//!
//! - [`StrongChunk::layout_sized`] / [`StrongChunk::init_sized`]：Sized 数据，无 meta；
//! - [`StrongChunk::layout_unsized`] / [`StrongChunk::init_unsized`]：`?Sized` 数据，
//!   带一个直接存放每对象元数据的槽；
//! - `P = ()` 表示没有 Predrop；其他 `P`（包括零尺寸钩子）由 [`TrPredrop::IS_ABSENT`]
//!   判定为存在。
//!
//! [`StrongChunk<T, P>`] 自身只是头部视图（`base_` + 零尺寸类型标记），不描述实际布局。
//!
//! # 类型擦除
//!
//! 强块头部保存 `type_id_`，它的值是合成类型 [`ChunkTy<T, P>`] 的
//! [`ConstTypeId`]。`RootScope::metas_` 用同一个 id 登记 vtable，析构入口因此能同时
//! 单态化 `T` 与 `P`；每对象 metadata 仍由 chunk 自己保存，类型表只放类型级信息。
//!
//! # `refc_` 的高位
//!
//! 最高位表示是否存在 meta 槽，次高位表示是否存在 Predrop 对象，低 30 位是强计数。
//!
//! # `pool_order_`
//!
//! 保存本块在所属 `StrongPool` 中的起始 cell 序号，供线程安全池在归还时反推池地址并
//! 取锁。

use core::{
    alloc::Layout,
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicU32, Ordering},
};

use typeid::ConstTypeId;

use crate::{
    abs_::TrPredrop,
    scope_inner_::PoolIndex,
    weak_::{WeakSlot, data_ptr_of_, meta_from_raw_},
};

/// 指针宽度；`dat_offset_` 以它为单位。
const PTR_SIZE: usize = mem::size_of::<usize>();

/// 强块类型擦除的合成键：`(T, P)` 共同决定 vtable 条目。
///
/// `T` 可能 `?Sized`，所以键类型本身必须是 `Sized`；`*const T` 恰好把 `T` 收进一个
/// 指针宽度的类型参数里，而 `P` 直接作为第二项。类型表用这个键区分析构入口与 Predrop
/// 入口，因此 `T` / `P` 都能被正确单态化。
pub(crate) struct ChunkTy<T: ?Sized, P>(PhantomData<(*const T, P)>);

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 强块头部

/// 强块与类型无关的公共头部。
///
/// 布局固定为：`refc_`（4 字节） + `pool_order_`（2 字节） + `dat_offset_`（2 字节）
/// + `weak_chunk_`（一个指针） + `type_id_`（一个指针宽度）。
///
/// 在 64 位平台上恰好是三个指针宽度；`type_id_` 正好占用了旧 `drop_` 指针的位置。
#[repr(C)]
pub(crate) struct StrongChunkBase {
    /// 强引用计数 + 两个结构标志位。
    ///
    /// - 最高位：是否存在 meta 槽；
    /// - 次高位：是否存在 Predrop 对象；
    /// - 其余 30 位：强引用计数。
    refc_: AtomicU32,

    /// 在所属 `StrongPool` 中的起始 cell 序号；归还时用于反推池地址并取锁。
    pool_order_: PoolIndex,

    /// 数据区相对 `StrongChunkBase` 首址的偏移，单位是指针宽度。
    dat_offset_: PoolIndex,

    /// 指回本块的身份弱槽位。
    weak_chunk_: NonNull<WeakSlot<()>>,

    /// 类型擦除键：合成类型 [`ChunkTy<T, P>`] 的 [`ConstTypeId`]。
    ///
    /// 析构路径拿它去 `RootScope::metas_` 查 vtable；强块本身不保存析构函数指针。
    type_id_: ConstTypeId,
}

impl StrongChunkBase {
    /// 最高位：存在 meta 槽。
    pub(crate) const REFC_HAS_META: u32 = 1 << 31;

    /// 次高位：存在 Predrop 对象。
    pub(crate) const REFC_HAS_PREDROP: u32 = 1 << 30;

    /// 低位：强引用计数掩码。
    pub(crate) const REFC_COUNT_MASK: u32 = !(Self::REFC_HAS_META | Self::REFC_HAS_PREDROP);

    /// 头部自身的大小。
    pub(crate) const THIS_SIZE: usize = mem::size_of::<Self>();

    /// 构造空白头部；随后必须由 typed 工厂写入真实布局。
    pub(crate) const fn empty_(weak_chunk: NonNull<WeakSlot<()>>, type_id: ConstTypeId) -> Self {
        StrongChunkBase {
            refc_: AtomicU32::new(0),
            pool_order_: 0,
            dat_offset_: 0,
            weak_chunk_: weak_chunk,
            type_id_: type_id,
        }
    }

    /// 本块的身份弱槽位。
    #[inline]
    pub(crate) const fn weak_chunk(&self) -> NonNull<WeakSlot<()>> {
        self.weak_chunk_
    }

    /// 本块用于查 vtable 的类型擦除键。
    #[inline]
    pub(crate) const fn type_id_(&self) -> ConstTypeId {
        self.type_id_
    }

    /// 本块在所属 `StrongPool` 中的起始 cell 序号。
    #[inline]
    pub(crate) const fn pool_order_(&self) -> PoolIndex {
        self.pool_order_
    }

    /// 数据区相对头部首址的偏移（指针单位）。
    #[inline]
    pub(crate) const fn dat_offset_(&self) -> PoolIndex {
        self.dat_offset_
    }

    /// 数据区相对头部首址的偏移（字节）。
    #[inline]
    pub(crate) const fn dat_offset_bytes_(&self) -> usize {
        self.dat_offset_ as usize * PTR_SIZE
    }

    /// 数据区起址（thin pointer）。
    ///
    /// 类型擦除析构只需要这个地址与每对象 metadata；它不需要也不应依赖 `StrongChunk`
    /// 首址是否仍然有效。
    #[inline]
    pub(crate) fn data_addr_(&self) -> *mut () {
        // SAFETY: dat_offset_ 由工厂写入，指向本块数据区
        unsafe {
            (self as *const Self as *mut u8)
                .add(self.dat_offset_bytes_())
                .cast()
        }
    }

    /// 是否存在 meta 槽。
    #[inline]
    pub(crate) fn has_meta_(&self) -> bool {
        self.refc_.load(Ordering::Acquire) & Self::REFC_HAS_META != 0
    }

    /// 是否存在 Predrop 对象。
    #[inline]
    pub(crate) fn has_predrop_(&self) -> bool {
        self.refc_.load(Ordering::Acquire) & Self::REFC_HAS_PREDROP != 0
    }

    /// 读取 meta 槽里的**每对象** `?Sized` 元数据裸字。
    ///
    /// 该槽直接保存 `T::Metadata` 按机器字搬运后的内容，不再指向 `RootScope` 里的另一个
    /// 指针字。没有 meta 槽时返回 [`None`]。
    #[inline]
    pub(crate) fn meta_raw_(&self) -> Option<*const ()> {
        if !self.has_meta_() {
            return Option::None;
        }
        // SAFETY: 无 meta 时不存在该槽；有 meta 时它紧跟头部
        let slot =
            unsafe { (self as *const Self as *const u8).add(Self::THIS_SIZE) as *const *const () };
        Option::Some(unsafe { *slot })
    }

    /// 数据的 `?Sized` 元数据。
    ///
    /// 没有 meta 槽时返回空指针。返回值是 `T::Metadata` 的裸字，可经
    /// [`meta_from_raw_`] 还原。
    #[inline]
    pub(crate) fn pointee_meta_(&self) -> *const () {
        self.meta_raw_().unwrap_or(ptr::null())
    }

    /// 按 `U` 重建数据引用；调用方必须保证数据仍然活着。
    ///
    /// # Safety
    ///
    /// 数据必须尚未析构，且 `U` 必须与当初分配的 `T` 是同一个类型。
    pub(crate) unsafe fn data_ref_as<U: ?Sized + ptr::Pointee>(&self) -> &U {
        // SAFETY: 头部首址就是强块首址；元数据来自 meta 槽
        let data =
            unsafe { data_ptr_of_::<U>(self as *const Self as *mut u8, self.pointee_meta_()) };
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
        let data = unsafe { data_ptr_of_::<U>(self as *mut Self as *mut u8, self.pointee_meta_()) };
        // SAFETY: 由调用方保证数据尚未析构且独占
        unsafe { &mut *data }
    }

    /// 当前强引用计数（不含两个标志位）。
    #[inline]
    pub(crate) fn strong_count(&self) -> u32 {
        self.refc_.load(Ordering::Acquire) & Self::REFC_COUNT_MASK
    }

    /// 增加强引用计数，返回增加后的计数。
    #[inline]
    pub(crate) fn incr_strong_count(&self) -> u32 {
        let old = self.refc_.fetch_add(1, Ordering::AcqRel);
        (old & Self::REFC_COUNT_MASK) + 1
    }

    /// 减少强引用计数，返回减少后的计数。
    #[inline]
    pub(crate) fn decr_strong_count(&self) -> u32 {
        let old = self.refc_.fetch_sub(1, Ordering::AcqRel);
        (old & Self::REFC_COUNT_MASK) - 1
    }

    /// 把强计数清零，但保留两个结构标志位。
    #[inline]
    pub(crate) fn reset_strong_count(&self) {
        self.refc_
            .fetch_and(!Self::REFC_COUNT_MASK, Ordering::Release);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 带类型的强块视图

/// Arena 中强块的**头部视图**与布局工厂。
///
/// 它本身只包含 `StrongChunkBase` 与零尺寸类型标记，因此指针地址与头部首址相同。
/// 真实布局（是否插 meta 槽、Predrop 对象、数据偏移）由 `layout_*` / `init_*` 工厂
/// 决定，不能直接从本 struct 的字段推导。
#[repr(C)]
pub(crate) struct StrongChunk<T, P = ()>
where
    T: ?Sized,
    P: TrPredrop<T>,
{
    base_: StrongChunkBase,
    _marker_: PhantomData<(*const T, *const P)>,
}

impl<T, P> StrongChunk<T, P>
where
    T: ?Sized,
    P: TrPredrop<T>,
{
    /// 带 meta 槽的布局（`T: ?Sized`）。
    ///
    /// # Errors
    ///
    /// 布局溢出或对齐非法时返回 [`None`]。
    pub(crate) fn layout_unsized(data_layout: Layout) -> Option<(Layout, PoolIndex)> {
        Self::layout_with(true, data_layout)
    }

    fn layout_with(has_meta: bool, data_layout: Layout) -> Option<(Layout, PoolIndex)> {
        let mut header = Layout::new::<StrongChunkBase>();
        if has_meta {
            header = header.extend(Layout::new::<*const ()>()).ok()?.0;
        }
        if !P::IS_ABSENT {
            header = header.extend(Layout::new::<P>()).ok()?.0;
        }
        let (layout, data_offset) = header.extend(data_layout).ok()?;
        let rounded = data_offset
            .checked_add(PTR_SIZE - 1)
            .map(|value| value & !(PTR_SIZE - 1))?;
        let extra = rounded - data_offset;
        let size = layout.size().checked_add(extra)?;
        let layout = Layout::from_size_align(size, layout.align()).ok()?;
        Option::Some((layout, (rounded / PTR_SIZE) as PoolIndex))
    }

    /// `T` 是否需要 meta 槽：`Sized` 的元数据宽度为 0，`?Sized` 则非 0。
    ///
    /// 分配层拿不到 `T` 的具体形状，只能靠这个判断在 [`StrongChunk::layout_auto`] /
    /// [`StrongChunk::init_auto`] 里自动选对工厂。
    pub(crate) fn needs_meta_() -> bool {
        mem::size_of::<<T as ptr::Pointee>::Metadata>() != 0
    }

    /// 依 `T` 是否 `?Sized` 自动选择布局，避免分配层分别调用 `layout_sized` /
    /// `layout_unsized`。
    ///
    /// # Errors
    ///
    /// 布局溢出或对齐非法时返回 [`None`]。
    pub(crate) fn layout_auto(data_layout: Layout) -> Option<(Layout, PoolIndex)> {
        Self::layout_with(Self::needs_meta_(), data_layout)
    }

    /// Predrop 对象在块内的字节偏移；`P = ()` 时无意义。
    fn predrop_offset_(has_meta: bool) -> usize {
        let start = StrongChunkBase::THIS_SIZE + if has_meta { PTR_SIZE } else { 0 };
        let align = mem::align_of::<P>();
        (start + align - 1) & !(align - 1)
    }

    /// 本块用于类型擦除的合成键。
    ///
    /// 键覆盖 `T` 与 `P`：同一个 `T` 配不同 Predrop 类型必须是不同条目，否则析构入口
    /// 无法单态化出正确的 `P::pre_drop`。`RootScope::ensure_chunk_meta_` 用同一个函数
    /// 生成查表键，两边必须始终一致。
    #[inline]
    pub(crate) const fn type_id_() -> ConstTypeId {
        ConstTypeId::of::<ChunkTy<T, P>>()
    }

    /// 数据的 `?Sized` 元数据字；没有 meta 槽时为空。
    #[inline]
    pub(crate) fn pointee_meta_(&self) -> *const () {
        self.base_.pointee_meta_()
    }

    /// 本块是否带 meta 槽。
    #[inline]
    pub(crate) fn has_meta_(&self) -> bool {
        self.base_.has_meta_()
    }

    /// 本块是否带 Predrop 对象。
    #[inline]
    pub(crate) fn has_predrop_(&self) -> bool {
        self.base_.has_predrop_()
    }

    /// 本块的身份弱槽位。
    #[inline]
    pub(crate) fn weak_chunk(&self) -> NonNull<WeakSlot<()>> {
        self.base_.weak_chunk()
    }

    /// 只读头部视图。
    #[inline]
    pub(crate) fn base_(&self) -> &StrongChunkBase {
        &self.base_
    }

    /// 数据区起址。
    #[inline]
    pub(crate) fn data_ptr(&self) -> *mut T {
        let base = self as *const Self as *const u8;
        // SAFETY: dat_offset_ 由工厂写入，指向本块数据区
        let data = unsafe { base.add(self.base_.dat_offset_bytes_()) };
        let meta = unsafe { meta_from_raw_::<T>(self.base_.pointee_meta_()) };
        // data 已按布局对齐；元数据来自本块自己的 meta 槽
        ptr::from_raw_parts_mut::<T>(data as *mut (), meta)
    }

    /// 读出数据区引用。调用方必须自行保证数据仍然活着。
    ///
    /// # Safety
    ///
    /// 仅当 `DataState` 表明数据尚未析构时才可调用。
    #[inline]
    pub(crate) unsafe fn data_ref_unchecked(&self) -> &T {
        // SAFETY: 由调用方保证数据仍然活着
        unsafe { &*self.data_ptr() }
    }

    /// Predrop 对象引用；没有时返回 [`None`]。
    #[inline]
    pub(crate) fn predrop_(&self) -> Option<&P> {
        if P::IS_ABSENT {
            return Option::None;
        }
        let base = self as *const Self as *const u8;
        let offset = Self::predrop_offset_(self.base_.has_meta_());
        // SAFETY: Predrop 槽由工厂按同一偏移写入，且对象就地存活
        Option::Some(unsafe { &*base.add(offset).cast::<P>() })
    }

    /// 当前强引用计数。
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
    pub(crate) fn try_get_data(&self) -> Option<&T> {
        if !self.is_data_alive() {
            return Option::None;
        }
        // SAFETY: 状态表明数据尚未析构
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
        // SAFETY: 弱槽位必然比强块活得久
        let weak = unsafe { self.base_.weak_chunk().as_ref() };
        weak.data_state().is_data_alive()
    }

    /// 写入头部、可选 meta 字、可选 Predrop 对象并绑定身份。
    ///
    /// `dat_offset` 必须来自同一形状的 `layout_*`。`has_meta` 区分 Sized / `?Sized`
    /// 工厂；`meta` 只在 `has_meta == true` 时使用，它是 `T::Metadata` 的裸字（由
    /// [`crate::weak_::meta_to_raw_`] 搬运），不是指向类型表的指针。
    ///
    /// # Safety
    ///
    /// `base` 必须指向按对应布局分配、尚未初始化的块；`weak_chunk` 必须是身份槽位；
    /// `meta` 必须是同一个 `T` 的元数据裸字。
    unsafe fn init_with(
        base: *mut Self,
        weak_chunk: NonNull<WeakSlot<()>>,
        pool_order: PoolIndex,
        has_meta: bool,
        meta: *const (),
        predrop: P,
        dat_offset: PoolIndex,
    ) {
        let mut flags = 0u32;
        if has_meta {
            flags |= StrongChunkBase::REFC_HAS_META;
        }
        if !P::IS_ABSENT {
            flags |= StrongChunkBase::REFC_HAS_PREDROP;
        }
        let base_bytes = base as *mut u8;
        // SAFETY: 调用方保证内存足够且尚未初始化
        unsafe {
            (base as *mut StrongChunkBase).write(StrongChunkBase {
                refc_: AtomicU32::new(flags),
                pool_order_: pool_order,
                dat_offset_: dat_offset,
                weak_chunk_: weak_chunk,
                type_id_: Self::type_id_(),
            });
            let slot = base_bytes.add(StrongChunkBase::THIS_SIZE);
            if has_meta {
                slot.cast::<*const ()>().write(meta);
            }
            if !P::IS_ABSENT {
                let offset = Self::predrop_offset_(has_meta);
                base_bytes.add(offset).cast::<P>().write(predrop);
            }
        }
        // SAFETY: weak_chunk 是本块的身份槽位，且此刻独占使用
        let weak = unsafe { &mut *(weak_chunk.as_ptr() as *mut WeakSlot<T>) };
        weak.set_strong_chunk(base as *mut StrongChunkBase);
    }

    /// `?Sized` 数据的工厂：meta 槽直接保存每对象元数据裸字。
    ///
    /// # Safety
    ///
    /// 同 [`StrongChunk::init_with`]；`meta` 必须是同一个 `T` 的元数据裸字。
    pub(crate) unsafe fn init_unsized(
        base: *mut Self,
        weak_chunk: NonNull<WeakSlot<()>>,
        pool_order: PoolIndex,
        meta: *const (),
        predrop: P,
        dat_offset: PoolIndex,
    ) {
        // SAFETY: 由调用方保证
        unsafe {
            Self::init_with(
                base, weak_chunk, pool_order, true, meta, predrop, dat_offset,
            )
        };
    }

    /// 依 `T` 是否 `?Sized` 自动选择初始化形状。
    ///
    /// `meta` 只在 [`StrongChunk::needs_meta_`] 为 `true` 时写入；`Sized` 时传空指针即可。
    ///
    /// # Safety
    ///
    /// 同 [`StrongChunk::init_with`]；`meta` 必须与 `T` 匹配。
    pub(crate) unsafe fn init_auto(
        base: *mut Self,
        weak_chunk: NonNull<WeakSlot<()>>,
        pool_order: PoolIndex,
        meta: *const (),
        predrop: P,
        dat_offset: PoolIndex,
    ) {
        // SAFETY: 由调用方保证
        unsafe {
            Self::init_with(
                base,
                weak_chunk,
                pool_order,
                Self::needs_meta_(),
                meta,
                predrop,
                dat_offset,
            )
        };
    }

    /// 消费内联的 Predrop 对象并执行钩子；`P = ()` 时什么都不做。
    ///
    /// 这个操作必须知道强块首址，因为 `P` 对象就内联在强块头部与数据区之间；但用户数据
    /// 本身的析构不依赖它。
    ///
    /// # Safety
    ///
    /// 数据必须仍然活着，且本方法对同一个块只能调用一次（Predrop 对象会被消费）。
    pub(crate) unsafe fn run_predrop_(&mut self) {
        if P::IS_ABSENT {
            return;
        }
        let data = self.data_ptr();
        let base = self as *const Self as *const u8;
        let offset = Self::predrop_offset_(self.base_.has_meta_());
        // SAFETY: Predrop 槽由工厂写入；消费后不再使用
        let predrop = unsafe { ptr::read(base.add(offset).cast::<P>()) };
        predrop.pre_drop(unsafe { &mut *data });
    }

    /// typed 析构：先消费 Predrop 对象执行钩子，再析构数据。
    ///
    /// # Safety
    ///
    /// 必须先由状态机认领销毁，且恰好调用一次。
    pub(crate) unsafe fn drop_data_(&mut self) {
        // SAFETY: 由调用方保证先认领销毁且恰好一次
        unsafe { self.run_predrop_() };
        let data = self.data_ptr();
        // SAFETY: 钩子必须把值留在可析构状态
        unsafe { ptr::drop_in_place(data) };
    }
}

impl<T, P> StrongChunk<T, P>
where
    T: Sized,
    P: TrPredrop<T>,
{
    /// Sized 数据的布局：没有 meta 槽。
    ///
    /// # Errors
    ///
    /// 布局溢出或对齐非法时返回 [`None`]。
    pub(crate) fn layout_sized(data_layout: Layout) -> Option<(Layout, PoolIndex)> {
        Self::layout_with(false, data_layout)
    }

    /// Sized 数据的工厂：没有 meta 槽。
    ///
    /// # Safety
    ///
    /// 同 [`StrongChunk::init_with`]。
    pub(crate) unsafe fn init_sized(
        base: *mut Self,
        weak_chunk: NonNull<WeakSlot<()>>,
        pool_order: PoolIndex,
        predrop: P,
        dat_offset: PoolIndex,
    ) {
        // SAFETY: 由调用方保证
        unsafe {
            Self::init_with(
                base,
                weak_chunk,
                pool_order,
                false,
                ptr::null(),
                predrop,
                dat_offset,
            )
        };
    }
}

impl<T> StrongChunk<T>
where
    T: Sized,
{
    /// 方便测试/简单路径：没有 meta、没有 Predrop，就地写入 `value`。
    ///
    /// # Safety
    ///
    /// 同 [`StrongChunk::init_sized`]；本块必须足够容纳默认布局。
    pub(crate) unsafe fn init_with_value(
        base: *mut Self,
        weak_chunk: NonNull<WeakSlot<()>>,
        value: T,
    ) {
        let (_, dat_offset) = Self::layout_sized(Layout::new::<T>()).expect("默认布局不应溢出");
        // SAFETY: 由调用方保证内存与身份槽位
        unsafe { Self::init_sized(base, weak_chunk, 0, (), dat_offset) };
        let data = unsafe { &*base }.data_ptr();
        // SAFETY: 数据区已分配、对齐且尚未初始化；由调用方保证独占
        unsafe { data.write(value) };
    }
}
