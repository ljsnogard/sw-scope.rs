//! 对象分配管线：强池 bump 分配、树共享弱池取槽、就地构造与挂链。
//!
//! 这里的方法紧密共享 `chain_head_` / `chain_tail_` / `live_head_` / `live_tail_` 与
//! `allocator_` 这组字段，因此放在同一个文件里。

use core::{
    alloc::{AllocError, Layout},
    ptr::{self, NonNull},
};

use crate::{
    emplace_::TrEmplace,
    index_::Retain,
    strong_::{StrongChunk, StrongChunkBase, StrongPool},
    weak_::{PreDropRecord, WeakChunk, WeakPool, meta_to_raw_, resolve_},
};

use super::{DEFAULT_PAGE_SIZE, PoolIndex, RootExtension, ScopeInner};

impl<const CELL_SIZE: usize> ScopeInner<CELL_SIZE> {
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
        let root = self.root_extension_();
        let head = root.weak_pools_head_()?;
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
                // 尾池已满：开一个新池，登记所属 root 并接到链尾
                let new = WeakPool::<CELL_SIZE, RootExtension<CELL_SIZE>>::try_new_with_max_size(
                    DEFAULT_PAGE_SIZE,
                    self.allocator_,
                )
                .ok()?;
                // SAFETY: new 是刚初始化的独占池
                let new_ref = unsafe { new.as_ptr().as_mut() }?;
                // SAFETY: self 属于本树，池只把 root 存下来再原样交还
                new_ref.set_root(NonNull::from(root));
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
        // 先尝试回收已经静默的关闭域，再分配新对象
        self.reclaim_pending_();
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
        weak_ref.chunk_state().init_created();
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
        // 先尝试回收已经静默的关闭域，再分配新对象
        self.reclaim_pending_();
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
        weak_ref.chunk_state().init_created();
        weak_ref.incr_weak_count();
        self.push_live_(weak);
        Result::Ok(Retain::new(weak.cast()))
    }
}
