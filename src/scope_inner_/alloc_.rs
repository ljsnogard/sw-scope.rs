//! 对象分配管线：取弱槽位、分配强块、就地构造数据与身份绑定。
//!
//! # 新设计下的池归属
//!
//! 弱池（[`WeakPool`]）与强池（[`StrongPool`]）都由**具体的 `Scope`** 持有：
//! `ScopeInner` 分别保存两条池链（`weak_head_` / `weak_tail_`、`strong_head_` /
//! `strong_tail_`）。因此这里不再需要从任意域反推 root，也不存在树级共享弱池。
//!
//! # 分配顺序
//!
//! 每次放入对象都严格按"**先弱槽位，后强块**"执行：
//!
//! 1. 从本域弱池链摘一个 [`WeakSlot`]，它将成为对象的身份与生命周期状态；
//! 2. 按 [`StrongChunk`] 的布局从本域强池链分配强块，并把强块地址写回槽位；
//! 3. 数据就位后才把槽位置为 `Created` 并登记 `Retain` 的弱计数。
//!
//! 第 2 步失败时，刚取出的槽位会立即归还给所属弱池（[`ScopeInner::deallocate_weak_`]），
//! 因此不会留下"已分配但没有强块"的槽位。

use core::{
    alloc::{AllocError, Layout},
    mem,
    ptr::{self, NonNull},
};

use crate::{
    abs_::{TrEmplace, TrPredrop},
    index_::Retain,
    scope_inner_::{DEFAULT_CELL_SIZE, PoolIndex, ScopeInner},
    strong_::{MetaAddr, StrongChunk, StrongPool},
    weak_::{WeakPool, WeakSlot},
};

impl ScopeInner {
    /// 最新一块强池中尚未分配出去的字节数。
    ///
    /// 只统计链尾池：更早的池按 Scope 的既定策略不做局部复用，因此它们的剩余空间不必计入。
    pub(crate) fn free_size_(&self) -> usize {
        let Option::Some(tail) = self.strong_tail_ else {
            return 0usize;
        };
        // SAFETY: 池链上的池都由本域分配器分配且仍然存活
        unsafe { tail.as_ref() }.free_addr_().len()
    }

    /// 从本域强池链上分配一块满足 `layout` 的内存，并返回它的起始 cell 序号。
    ///
    /// 链尾池放不下时开一个新池接到链尾；起始 cell 序号要写进强块头部的
    /// `pool_order_`，归还时才能 O(1) 反推所属池。
    ///
    /// # Errors
    ///
    /// 需要的 cell 数超过 [`PoolIndex`] 上界，或底层分配器失败时返回 [`AllocError`]。
    pub(crate) fn allocate_(
        &mut self,
        layout: Layout,
    ) -> Result<(NonNull<[u8]>, PoolIndex), AllocError> {
        if let Option::Some(tail) = self.strong_tail_ {
            // SAFETY: 池链上的池都由本域分配器分配且仍然存活，本域独占
            let pool = unsafe { tail.as_ptr().as_mut() }.ok_or(AllocError)?;
            if let Result::Ok(mem) = pool.allocate_with_order_(layout) {
                return Result::Ok(mem);
            }
        }
        // 新池容量取"放得下这一块（含对齐填充）"与默认页大小的较大者。
        // 对齐填充最多 layout.align() - 1 字节，必须一起算进 cell 数。
        let padding = layout.align().saturating_sub(1);
        let need_bytes = layout.size().checked_add(padding).ok_or(AllocError)?;
        let need_cells = need_bytes.div_ceil(DEFAULT_CELL_SIZE).max(1);
        let cell_count = need_cells.max(self.page_size_.max(DEFAULT_CELL_SIZE) / DEFAULT_CELL_SIZE);
        if cell_count > PoolIndex::MAX as usize {
            return Result::Err(AllocError);
        }
        let pool = StrongPool::try_new_(
            cell_count as PoolIndex,
            // SAFETY: allocator_ 指向本实体尾部、在实体析构前一直有效
            unsafe { self.allocator_.as_ref() },
            self.strong_tail_,
        )?;
        if self.strong_head_.is_none() {
            self.strong_head_ = Option::Some(pool);
        }
        self.strong_tail_ = Option::Some(pool);
        // SAFETY: pool 是刚初始化的独占池
        unsafe { pool.as_ptr().as_mut() }
            .ok_or(AllocError)?
            .allocate_with_order_(layout)
            .map_err(|_| AllocError)
    }

    /// 从本域弱池链上取一个槽位；链尾池满了就开一个新池接到链尾。
    ///
    /// 新池会把自己的所属域记为 `self`（[`WeakPool::set_scope`]），供清盘与池链回收
    /// 路径反查。返回 [`None`] 只会在底层分配器失败时发生。
    pub(crate) fn allocate_weak_(&mut self) -> Option<NonNull<WeakSlot<()>>> {
        if let Option::Some(tail) = self.weak_tail_ {
            // SAFETY: 池链上的池都由本域分配器分配且仍然存活，本域独占
            let pool = unsafe { tail.as_ptr().as_mut() }?;
            if pool.free_count() != 0 {
                let index = pool.allocate()?;
                return Option::Some(Self::slot_at_(pool, index));
            }
        }
        // 尾池已满（或还没有池）：开一个新池，登记所属域并接到链尾
        // SAFETY: allocator_ 指向本实体尾部、在实体析构前一直有效
        let alloc = unsafe { self.allocator_.as_ref() };
        let new = WeakPool::try_new_with_max_size(self.page_size_, alloc).ok()?;
        // SAFETY: new 是刚初始化的独占池
        let new_ref = unsafe { new.as_ptr().as_mut() }?;
        // 池只保存所属域的地址，不取得所有权
        new_ref.set_scope(NonNull::from(&mut *self));
        match self.weak_tail_ {
            Option::Some(mut tail) => {
                // SAFETY: tail 与 new 是两块不同的池
                unsafe { tail.as_mut() }.link_siblings(new_ref);
            }
            Option::None => {
                self.weak_head_ = Option::Some(new);
            }
        }
        self.weak_tail_ = Option::Some(new);
        let index = new_ref.allocate()?;
        Option::Some(Self::slot_at_(new_ref, index))
    }

    /// 把一个尚未投入使用的弱槽位归还给所属池。
    ///
    /// 这是分配失败时的回滚路径：槽位尚未写入状态与强块指针，直接还给空闲链即可。
    fn deallocate_weak_(&mut self, weak: NonNull<WeakSlot<()>>) {
        let Option::Some(pool) = WeakPool::of_slot_(weak) else {
            return;
        };
        // SAFETY: weak 是本域从该池取出的槽位，池仍然存活
        let Option::Some(index) = unsafe { pool.as_ref() }.index_of_(weak) else {
            return;
        };
        // SAFETY: 同上；本域独占整条池链
        let Option::Some(pool_ref) = (unsafe { pool.as_ptr().as_mut() }) else {
            return;
        };
        let _ = pool_ref.deallocate(index);
    }

    /// 把刚取出的槽位正式投入使用：状态置 `Created`，并给 `Retain` 登记一次弱计数。
    ///
    /// 此刻槽位尚未发布给其它线程，因此两步之间不存在可见性问题。
    fn init_slot_created_(weak: NonNull<WeakSlot<()>>) {
        // SAFETY: weak 是本域刚取出的槽位，尚未发布
        let slot = unsafe { weak.as_ref() };
        slot.chunk_state().init_created();
        slot.incr_weak_count();
    }

    /// 取弱池内第 `index` 个槽位。
    ///
    /// # Safety
    ///
    /// `index` 必须小于池的 `capacity_`。
    fn slot_at_(pool: &mut WeakPool, index: PoolIndex) -> NonNull<WeakSlot<()>> {
        let slots = pool.slots();
        // SAFETY: index 由 WeakPool::allocate 从空闲链摘出，必然小于 capacity_
        unsafe { NonNull::new_unchecked(slots.as_ptr().cast::<WeakSlot<()>>().add(index as usize)) }
    }

    /// 就地构造 `value` 并交出句柄（无对象级 `PreDrop`）。
    ///
    /// # Errors
    ///
    /// 弱池或强池分配失败时返回 [`AllocError`]。
    pub(crate) fn put_value_<T>(&mut self, value: T) -> Result<Retain<T>, AllocError> {
        self.put_value_with_(value, ())
    }

    /// 就地构造 `value` 并交出句柄，同时把 `predrop` 存进强块头部。
    ///
    /// `predrop` 是 `P: TrPredrop<T>` 的对象；`P = ()` 表示没有钩子，此时不占头部空间。
    ///
    /// # Errors
    ///
    /// 弱池或强池分配失败时返回 [`AllocError`]。
    pub(crate) fn put_value_with_<T, P>(
        &mut self,
        value: T,
        predrop: P,
    ) -> Result<Retain<T>, AllocError>
    where
        T: Sized,
        P: TrPredrop<T>,
    {
        // 1) 先取身份槽位
        let weak = self.allocate_weak_().ok_or(AllocError)?;
        // 2) 再按强块布局分配；失败时把槽位还回去
        let Option::Some((chunk_layout, dat_offset)) =
            StrongChunk::<T, P>::layout_sized(Layout::new::<T>())
        else {
            self.deallocate_weak_(weak);
            return Result::Err(AllocError);
        };
        let (mem, pool_order) = match self.allocate_(chunk_layout) {
            Result::Ok(allocated) => allocated,
            Result::Err(err) => {
                self.deallocate_weak_(weak);
                return Result::Err(err);
            }
        };
        let base = mem.as_ptr() as *mut u8 as *mut StrongChunk<T, P>;
        // SAFETY: mem 满足 chunk_layout 且本域独占；weak 是本域刚取出的槽位
        unsafe {
            StrongChunk::<T, P>::init_sized(base, weak, pool_order, predrop, dat_offset);
            (*base).data_ptr().write(value);
        }
        Self::init_slot_created_(weak);
        Result::Ok(Retain::new(weak.cast()))
    }

    /// 就地构造一个（可能 `?Sized` 的）值并交出句柄。
    ///
    /// 与 [`ScopeInner::put_value_with_`] 的区别：数据区布局由调用方给出，构造由
    /// `emplace` 完成，`?Sized` 元数据由 [`TrEmplace::meta_`] 提供。
    ///
    /// `meta_addr` 是该元数据在 `RootScope` 元信息表中的稳定地址，`Sized` 数据传
    /// [`None`]（`RootScope` 的元信息表由 Scope 层持有，因此这一步必须由能访问 root
    /// 的调用方完成）。
    ///
    /// # Safety
    ///
    /// - 调用方必须满足 [`TrEmplace::emplace`] 的安全契约；
    /// - `meta_addr` 必须与 `emplace.meta_()` 给出的元数据匹配，并在强块存活期间保持稳定。
    pub(crate) unsafe fn emplace_value_with_<TyEmp, P>(
        &mut self,
        layout: Layout,
        emplace: TyEmp,
        predrop: P,
        meta_addr: Option<MetaAddr>,
    ) -> Result<Retain<TyEmp::Target>, AllocError>
    where
        TyEmp: TrEmplace,
        P: TrPredrop<TyEmp::Target>,
    {
        let meta: <TyEmp::Target as ptr::Pointee>::Metadata = emplace.meta_().into();
        let needs_meta = StrongChunk::<TyEmp::Target, P>::needs_meta_();
        debug_assert_eq!(
            needs_meta,
            meta_addr.is_some(),
            "?Sized 数据必须提供 RootScope 元信息地址，Sized 数据不应提供",
        );
        // 1) 先取身份槽位
        let weak = self.allocate_weak_().ok_or(AllocError)?;
        // 2) 按数据区布局求整块布局；失败时把槽位还回去
        let Option::Some((chunk_layout, dat_offset)) =
            StrongChunk::<TyEmp::Target, P>::layout_auto(layout)
        else {
            self.deallocate_weak_(weak);
            return Result::Err(AllocError);
        };
        let (mem, pool_order) = match self.allocate_(chunk_layout) {
            Result::Ok(allocated) => allocated,
            Result::Err(err) => {
                self.deallocate_weak_(weak);
                return Result::Err(err);
            }
        };
        let base = mem.as_ptr() as *mut u8;
        let data_offset_bytes = dat_offset as usize * mem::size_of::<usize>();
        // SAFETY: data_offset_bytes 由同一布局给出，落在刚分配的内存内
        let data = unsafe { base.add(data_offset_bytes) };
        // arena 先建好胖指针，再交回 emplace 就地写入
        let place = ptr::from_raw_parts_mut::<TyEmp::Target>(data.cast::<()>(), meta);
        // SAFETY: 布局匹配、数据尚未初始化，且由调用方保证 emplace 的契约
        unsafe { emplace.emplace(layout, place) };
        let chunk = base as *mut StrongChunk<TyEmp::Target, P>;
        // SAFETY: 数据刚由 emplace 就地构造完毕；weak 是本域刚取出的槽位；
        // meta_addr 由调用方保证与 T 匹配且稳定
        unsafe {
            StrongChunk::<TyEmp::Target, P>::init_auto(
                chunk,
                weak,
                pool_order,
                meta_addr.unwrap_or(ptr::null()),
                predrop,
                dat_offset,
            );
        }
        Self::init_slot_created_(weak);
        Result::Ok(Retain::new(weak.cast()))
    }
}

#[cfg(test)]
mod tests_ {
    use core::{
        alloc::{AllocError, Allocator, Layout},
        mem,
        ptr::NonNull,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use std::alloc::Global;

    use super::*;
    use crate::{abs_::IntoEmplace, scope_inner_::DEFAULT_PAGE_SIZE, weak_::DataState};

    static GLOBAL: Global = Global;

    /// 允许继续通过的分配次数；`usize::MAX` 表示不限制。
    static ALLOW_ALLOCS: AtomicUsize = AtomicUsize::new(usize::MAX);

    /// 包一层 [`Global`]，可以按次数制造分配失败。
    struct CountingAlloc;

    // SAFETY: 这里只把分配 / 释放原样转交给 `Global`，不改变其安全契约
    unsafe impl Allocator for CountingAlloc {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            match ALLOW_ALLOCS.try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                if left == usize::MAX {
                    Option::Some(usize::MAX)
                } else {
                    left.checked_sub(1)
                }
            }) {
                Result::Ok(_) => GLOBAL.allocate(layout),
                Result::Err(_) => Result::Err(AllocError),
            }
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            // SAFETY: ptr 与 layout 来自上面的 allocate 原样转交给 Global 的那次分配
            unsafe { GLOBAL.deallocate(ptr, layout) }
        }
    }

    static COUNTING: CountingAlloc = CountingAlloc;

    /// 构造一个只带分配器的空域，供分配管线测试使用。
    fn empty_scope_<A>(alloc: &'static A) -> ScopeInner
    where
        A: Allocator + Send + Sync,
    {
        let allocator: NonNull<dyn Allocator + Send + Sync> = NonNull::from(alloc);
        ScopeInner {
            flags_: AtomicUsize::new(0),
            weak_head_: Option::None,
            weak_tail_: Option::None,
            strong_head_: Option::None,
            strong_tail_: Option::None,
            allocator_: allocator,
            page_size_: DEFAULT_PAGE_SIZE,
        }
    }

    /// 测试放入对象时先取弱槽位、再分配强块，并在强池分配失败时回滚槽位。
    /// Test that a put takes the weak slot first, then the strong chunk, and rolls the slot
    /// back when the strong-pool allocation fails.
    ///
    /// - 手段：用只放行一次分配的自定义分配器放入第一个值（弱池成功、强池失败），
    ///   再恢复分配并重试。
    ///   / How: a counting allocator allows exactly one allocation, so the weak pool succeeds
    ///   and the strong pool fails; allocations are then re-enabled and the put is retried.
    /// - 判断：首次返回 `Err`，弱池未用槽位且没有登记强池；重试成功，槽位状态为
    ///   `Created`、弱计数为 1、强块指针非空，`try_owning` 能读到原值。
    ///   / What: the first call returns `Err` with zero used slots and no strong pool; the
    ///   retry yields `Created`, weak count 1, a non-null strong chunk, and the original value.
    #[test]
    fn allocation_takes_weak_slot_first_and_rolls_back() {
        ALLOW_ALLOCS.store(1, Ordering::SeqCst);
        let mut inner = empty_scope_(&COUNTING);

        assert!(inner.put_value_(7u32).is_err(), "强池分配失败应向上报告");
        // SAFETY: 弱池由本测试独占
        let weak_tail = unsafe { inner.weak_tail_.expect("弱池应已创建").as_ref() };
        assert_eq!(weak_tail.used_count(), 0, "失败后槽位必须归还给弱池");
        assert!(inner.strong_head_.is_none(), "失败时不应登记强池");
        assert_eq!(inner.free_size_(), 0, "没有强池时可用空间为 0");

        ALLOW_ALLOCS.store(usize::MAX, Ordering::SeqCst);
        let retain = inner.put_value_(7u32).expect("恢复分配后应当成功");
        assert_eq!(*retain.try_owning().expect("数据应当可独占"), 7);

        // SAFETY: 强池、弱池都由本测试独占
        let weak_tail = unsafe { inner.weak_tail_.expect("弱池应存在").as_mut() };
        assert_eq!(weak_tail.used_count(), 1);
        let slots = weak_tail.slots();
        // SAFETY: 第一个槽位已分配，落在池内
        let slot = unsafe { slots.as_ref() }.first().expect("槽位应存在");
        assert_eq!(slot.data_state(), DataState::Created);
        assert_eq!(slot.weak_count(), 1, "Retain 应登记一次弱计数");
        assert!(slot.strong_chunk().is_some(), "强块地址应已写回槽位");
        assert!(inner.strong_head_.is_some(), "强池应已登记");
        assert!(inner.free_size_() > 0, "新强池应当还有可用空间");

        // 类型擦除析构尚未接通，先避免 `Retain::drop` 走到 `todo!()`
        mem::forget(retain);
    }

    /// 测试 `?Sized` 数据通过外部元信息地址原地构造，并按指针宽度定位数据区。
    /// Test in-place construction of unsized data through an external metadata address.
    ///
    /// - 手段：用 `IntoEmplace` 把一段 `[u32]` 拷进 arena，元数据（长度）由
    ///   `meta_to_raw_` 转成裸字并放进一个泄漏到静态区的指针字。
    ///   / How: copy a `[u32]` slice with `IntoEmplace`; the length metadata is turned into a
    ///   raw word by `meta_to_raw_` and stored in a leaked static pointer word.
    /// - 判断：`try_owning` 得到的切片与源数据完全相等，且槽位状态为 `Created`。
    ///   / What: the owned slice equals the source, and the slot state is `Created`.
    #[test]
    fn emplace_unsized_value_uses_meta_addr() {
        let mut inner = empty_scope_(&GLOBAL);
        let values = [3u32, 1, 4, 1, 5];
        let meta_word: &'static *const () = std::boxed::Box::leak(std::boxed::Box::new(
            crate::weak_::meta_to_raw_(core::ptr::metadata(&values[..])),
        ));
        let meta_addr = meta_word as *const *const () as MetaAddr;
        let emplace = IntoEmplace::<_, [u32]>::new(
            |_layout, place: *mut [u32]| {
                // SAFETY: place 由 arena 按 layout 划出且尚未初始化
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        values.as_ptr(),
                        place.cast::<u32>(),
                        values.len(),
                    )
                };
            },
            values.len(),
        );
        // SAFETY: 元信息地址由本测试保证稳定，emplace 不会越界
        let retain = unsafe {
            inner.emplace_value_with_(
                Layout::for_value(&values[..]),
                emplace,
                (),
                Option::Some(meta_addr),
            )
        }
        .expect("?Sized 放入应当成功");

        assert_eq!(&*retain.try_owning().expect("数据应当可独占"), &values[..]);

        // SAFETY: 弱池由本测试独占
        let weak_tail = unsafe { inner.weak_tail_.expect("弱池应存在").as_mut() };
        let slots = weak_tail.slots();
        // SAFETY: 第一个槽位已分配，落在池内
        let slot = unsafe { slots.as_ref() }.first().expect("槽位应存在");
        assert_eq!(slot.data_state(), DataState::Created);

        // 类型擦除析构尚未接通，先避免 `Retain::drop` 走到 `todo!()`
        mem::forget(retain);
    }

    /// 测试 `Sized` 数据走 `emplace_value_with_` 时不带 meta 指针，仍能正确就地构造。
    /// Test that Sized data emplaced through `emplace_value_with_` has no meta pointer but is
    /// still constructed correctly in place.
    ///
    /// - 手段：用 `IntoEmplace` 把一个 `u64` 就地写入 arena，`meta_addr` 传 `None`。
    ///   / How: write a `u64` into the arena with `IntoEmplace` and pass `None` as `meta_addr`.
    /// - 判断：`try_owning` 读到原值，且强块头部没有置 meta 标志位。
    ///   / What: `try_owning` reads the original value and the chunk header has no meta flag.
    #[test]
    fn emplace_sized_value_needs_no_meta_addr() {
        let mut inner = empty_scope_(&GLOBAL);
        let emplace = IntoEmplace::<_, u64>::new(
            |_layout, place: *mut u64| {
                // SAFETY: place 由 arena 按 layout 划出且尚未初始化
                unsafe { place.write(0xDEAD_BEEF) };
            },
            (),
        );
        // SAFETY: Sized 数据不需要元信息地址，emplace 不会越界
        let retain =
            unsafe { inner.emplace_value_with_(Layout::new::<u64>(), emplace, (), Option::None) }
                .expect("Sized 放入应当成功");
        assert_eq!(*retain.try_owning().expect("数据应当可独占"), 0xDEAD_BEEF);

        // SAFETY: 弱池由本测试独占
        let weak_tail = unsafe { inner.weak_tail_.expect("弱池应存在").as_mut() };
        let slots = weak_tail.slots();
        // SAFETY: 第一个槽位已分配，落在池内
        let slot = unsafe { slots.as_ref() }.first().expect("槽位应存在");
        // SAFETY: 强块由本次放入建立，地址非空
        let chunk = unsafe { slot.strong_chunk().expect("强块应已绑定").as_ref() };
        assert!(!chunk.has_meta_(), "Sized 数据不应带 meta 指针");

        // 类型擦除析构尚未接通，先避免 `Retain::drop` 走到 `todo!()`
        mem::forget(retain);
    }
}
