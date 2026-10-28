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
    strong_::{StrongChunk, StrongPool},
    weak_::{WeakPool, WeakSlot, meta_to_raw_},
};

impl ScopeInner {
    /// 在本域所属 root 的类型表里登记 `T` / `P` 组合。
    ///
    /// 低层分配管线会被单元测试直接调用而没有 root；那种情况下跳过登记（测试不进入类型
    /// 擦除析构路径）。生产路径上的 root 由 `Scope::new_in` 系列显式绑定。
    fn ensure_chunk_meta_<T: ?Sized, P: TrPredrop<T>>(&self) {
        if let Option::Some(root) = self.root_ {
            // SAFETY: 本域存活期间 root 不会被最终清理
            let _ = unsafe { root.as_ref() }.ensure_chunk_meta_::<T, P>();
        }
    }
}

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
    /// 新池会把自己的所属域记为 `self`（[`WeakPool::set_scope`]），供清盘与末槽归还
    /// 路径反查。返回 [`None`] 只会在底层分配器失败时发生。
    ///
    /// # 同步
    ///
    /// 取槽与“末槽归还后摘链”都必须在弱池链锁内完成：否则 `Retain` drop 线程可能在
    /// 本线程选中链尾池之后把这个空池摘链并释放，导致悬垂访问。链锁顺序为
    /// **域写锁 → 弱池链锁 → 池锁**。
    pub(crate) fn allocate_weak_(&mut self) -> Option<NonNull<WeakSlot<()>>> {
        // 先取 self 指针，避免与 `weak_chain_` 的不可变借用冲突
        let self_ptr = NonNull::from(&mut *self);
        let mut session = self.weak_chain_.lock_session();
        let mut chain = match session.lock().wait() {
            Result::Ok(guard) => guard,
            Result::Err(_) => unreachable!("weak chain 自旋锁不应失败"),
        };
        if let Option::Some(tail) = chain.tail_ {
            // SAFETY: 链尾池在链锁内仍然存活
            let pool = unsafe { tail.as_ref() };
            if pool.free_count() != 0 {
                // SAFETY: 链锁保证没有其它线程摘链；池锁由 allocate 内部获取
                let pool_mut = unsafe { tail.as_ptr().as_mut() }?;
                let index = pool_mut.allocate()?;
                return Option::Some(Self::slot_at_(pool_mut, index));
            }
        }
        // 尾池已满（或还没有池）：开一个新池，登记所属域并接到链尾。
        // 新池分配调用用户分配器；这是罕见路径，且必须在链锁内完成链修改。
        // SAFETY: allocator_ 指向本实体尾部、在实体析构前一直有效
        let alloc = unsafe { self.allocator_.as_ref() };
        let new = WeakPool::try_new_with_max_size(self.page_size_, alloc).ok()?;
        // 链锁内把新池接到链尾；pool.scope_ 由 push_back_ 一并写入
        chain.push_back_(new, self_ptr);
        // SAFETY: new 是刚初始化、独占的池，且已经挂在链上
        let new_mut = unsafe { new.as_ptr().as_mut() }?;
        let index = new_mut.allocate()?;
        Option::Some(Self::slot_at_(new_mut, index))
    }

    /// 把一个尚未投入使用的弱槽位归还给所属池。
    ///
    /// 这是分配失败时的回滚路径：槽位尚未写入状态与强块指针，直接还给空闲链即可。若
    /// 这次回滚让池变成空池（例如新池的第一个槽位就分配失败），则顺手把池摘链并释放，
    /// 避免留下永远不会再被使用的空池。
    pub(crate) fn deallocate_weak_(&mut self, weak: NonNull<WeakSlot<()>>) {
        let Option::Some(pool) = WeakPool::of_slot_(weak) else {
            return;
        };
        // SAFETY: weak 是本域从该池取出的槽位，池仍然存活
        let Option::Some(index) = unsafe { pool.as_ref() }.index_of_(weak) else {
            return;
        };
        let mut to_free = Option::None;
        {
            let mut session = self.weak_chain_.lock_session();
            let mut chain = match session.lock().wait() {
                Result::Ok(guard) => guard,
                Result::Err(_) => unreachable!("weak chain 自旋锁不应失败"),
            };
            // SAFETY: 链锁 + 池锁串行化；池仍在链上
            let Option::Some(pool_mut) = (unsafe { pool.as_ptr().as_mut() }) else {
                return;
            };
            if !pool_mut.deallocate(index) {
                return;
            }
            if pool_mut.live_count_() == 0 && pool_mut.try_mark_detached_() {
                chain.unlink_(pool);
                to_free = Option::Some(pool);
            }
        }
        if let Option::Some(pool) = to_free {
            // SAFETY: 已摘链且没有活槽位；池不再被任何线程访问
            let alloc = unsafe { self.allocator_.as_ref() };
            // SAFETY: 同上
            unsafe { WeakPool::dealloc_(pool, alloc) };
        }
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
        // 0) 类型级 vtable 与强块头部的 type_id 使用同一个合成键；登记在分配之后、
        //    写入数据之前完成。
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
        self.ensure_chunk_meta_::<T, P>();
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
    /// 每对象元数据会由 [`meta_to_raw_`] 搬成裸字、**直接写进强块自己的 meta 槽**；
    /// 类型表只登记类型级 vtable，不承载每对象长度。
    ///
    /// # Safety
    ///
    /// 调用方必须满足 [`TrEmplace::emplace`] 的安全契约，且 `emplace.meta_()` 必须与
    /// 数据区布局一致。
    pub(crate) unsafe fn emplace_value_with_<TyEmp, P>(
        &mut self,
        layout: Layout,
        emplace: TyEmp,
        predrop: P,
    ) -> Result<Retain<TyEmp::Target>, AllocError>
    where
        TyEmp: TrEmplace,
        P: TrPredrop<TyEmp::Target>,
    {
        let meta: <TyEmp::Target as ptr::Pointee>::Metadata = emplace.meta_().into();
        let meta_raw = meta_to_raw_(meta);
        debug_assert_eq!(
            StrongChunk::<TyEmp::Target, P>::needs_meta_(),
            !meta_raw.is_null(),
            "?Sized 数据的元数据裸字不应为空，Sized 数据应为空",
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
        self.ensure_chunk_meta_::<TyEmp::Target, P>();
        let chunk = base as *mut StrongChunk<TyEmp::Target, P>;
        // SAFETY: 数据刚由 emplace 就地构造完毕；weak 是本域刚取出的槽位；
        // meta_raw 与 T 的 metadata 同源，且已直接写进强块 meta 槽
        unsafe {
            StrongChunk::<TyEmp::Target, P>::init_auto(
                chunk, weak, pool_order, meta_raw, predrop, dat_offset,
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
        ScopeInner::empty_(allocator, DEFAULT_PAGE_SIZE)
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
        // 回滚把唯一的槽位还回去后池已经空，按新策略空池会立即摘链并释放，因此这里不再
        // 有弱池留给下一次放入。
        assert!(inner.weak_tail_().is_none(), "失败后空弱池应被摘链回收");
        assert!(inner.strong_head_.is_none(), "失败时不应登记强池");
        assert_eq!(inner.free_size_(), 0, "没有强池时可用空间为 0");

        ALLOW_ALLOCS.store(usize::MAX, Ordering::SeqCst);
        let retain = inner.put_value_(7u32).expect("恢复分配后应当成功");
        assert_eq!(*retain.try_owning().expect("数据应当可独占"), 7);

        // SAFETY: 强池、弱池都由本测试独占
        let weak_tail = unsafe { inner.weak_tail_().expect("弱池应存在").as_mut() };
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

    /// 测试 `?Sized` 数据的每对象元数据直接写进强块 meta 槽。
    /// Test that per-object metadata for unsized data is stored in the chunk meta slot itself.
    ///
    /// - 手段：用 `IntoEmplace` 把一段 `[u32]` 拷进 arena；元数据（长度）由
    ///   `meta_to_raw_` 转成裸字，由分配管线写进强块。
    ///   / How: copy a `[u32]` slice with `IntoEmplace`; the length metadata is turned into a
    ///   raw word by `meta_to_raw_` and written into the chunk by the allocation pipeline.
    /// - 判断：`try_owning` 得到的切片与源数据完全相等，槽位状态为 `Created`，且强块
    ///   meta 槽里正是同一个裸字、type_id 与 `StrongChunk<[u32], ()>` 一致。
    ///   / What: the owned slice equals the source, the slot state is `Created`, and the chunk
    ///   meta slot holds the same raw word with the expected synthetic type id.
    #[test]
    fn emplace_unsized_value_stores_metadata_in_chunk() {
        let mut inner = empty_scope_(&GLOBAL);
        let values = [3u32, 1, 4, 1, 5];
        let meta_raw = meta_to_raw_(core::ptr::metadata(&values[..]));
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
        // SAFETY: emplace 不会越界，元数据与数据区布局一致
        let retain =
            unsafe { inner.emplace_value_with_(Layout::for_value(&values[..]), emplace, ()) }
                .expect("?Sized 放入应当成功");

        assert_eq!(&*retain.try_owning().expect("数据应当可独占"), &values[..]);

        // SAFETY: 弱池由本测试独占
        let weak_tail = unsafe { inner.weak_tail_().expect("弱池应存在").as_mut() };
        let slots = weak_tail.slots();
        // SAFETY: 第一个槽位已分配，落在池内
        let slot = unsafe { slots.as_ref() }.first().expect("槽位应存在");
        assert_eq!(slot.data_state(), DataState::Created);
        // SAFETY: 强块由本次放入建立，地址非空
        let chunk = unsafe { slot.strong_chunk().expect("强块应已绑定").as_ref() };
        assert_eq!(chunk.meta_raw_(), Option::Some(meta_raw));
        assert_eq!(
            chunk.type_id_(),
            StrongChunk::<[u32]>::type_id_(),
            "强块必须保存合成类型 id"
        );

        // 低层测试没有初始化默认 root，直接 leak 掉句柄，避免进入类型擦除析构。
        mem::forget(retain);
    }

    /// 测试 `Sized` 数据走 `emplace_value_with_` 时不占 meta 槽，仍能正确就地构造。
    /// Test that Sized data emplaced through `emplace_value_with_` uses no meta slot but is
    /// still constructed correctly in place.
    ///
    /// - 手段：用 `IntoEmplace` 把一个 `u64` 就地写入 arena，元数据为 `()`。
    ///   / How: write a `u64` into the arena with `IntoEmplace` and use `()` metadata.
    /// - 判断：`try_owning` 读到原值，且强块头部没有置 meta 标志位。
    ///   / What: `try_owning` reads the original value and the chunk header has no meta flag.
    #[test]
    fn emplace_sized_value_has_no_meta_slot() {
        let mut inner = empty_scope_(&GLOBAL);
        let emplace = IntoEmplace::<_, u64>::new(
            |_layout, place: *mut u64| {
                // SAFETY: place 由 arena 按 layout 划出且尚未初始化
                unsafe { place.write(0xDEAD_BEEF) };
            },
            (),
        );
        // SAFETY: Sized 数据不需要元数据槽，emplace 不会越界
        let retain = unsafe { inner.emplace_value_with_(Layout::new::<u64>(), emplace, ()) }
            .expect("Sized 放入应当成功");
        assert_eq!(*retain.try_owning().expect("数据应当可独占"), 0xDEAD_BEEF);

        // SAFETY: 弱池由本测试独占
        let weak_tail = unsafe { inner.weak_tail_().expect("弱池应存在").as_mut() };
        let slots = weak_tail.slots();
        // SAFETY: 第一个槽位已分配，落在池内
        let slot = unsafe { slots.as_ref() }.first().expect("槽位应存在");
        // SAFETY: 强块由本次放入建立，地址非空
        let chunk = unsafe { slot.strong_chunk().expect("强块应已绑定").as_ref() };
        assert!(!chunk.has_meta_(), "Sized 数据不应占用 meta 槽");
        assert_eq!(chunk.type_id_(), StrongChunk::<u64>::type_id_());

        // 低层测试没有初始化默认 root，直接 leak 掉句柄，避免进入类型擦除析构。
        mem::forget(retain);
    }
}
