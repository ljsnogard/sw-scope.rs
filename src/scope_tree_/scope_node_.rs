//! 对象分配管线：强池 bump 分配、树共享弱池取槽、就地构造与挂链。
//!
//! 这里的方法紧密共享 `chain_head_` / `chain_tail_` / `live_head_` / `live_tail_` 与
//! `allocator_` 这组字段，因此放在同一个文件里。

use core::{
    alloc::{Allocator, AllocError, Layout},
    ptr::{self, NonNull},
    sync::atomic::AtomicUsize,
};

use crate::{
    emplace_::TrEmplace,
    smart_pointer_::Retain,
    strong_::{StrongChunk, StrongChunkHead, StrongPool},
    weak_::{PreDropRecord, WeakChunk, meta_to_raw_, resolve_},
};

use super::{
    DEFAULT_CELL_SIZE, DEFAULT_PAGE_SIZE,
    PoolIndex, TreeNodeBase,
    root_node_::{RootNodeExt, RootScope},
};

pub(crate) struct ScopeNode {
    node_base_: TreeNodeBase,
    scope_ext_: ScopeNodeExt,
}

impl AsRef<TreeNodeBase> for ScopeNode {
    #[inline]
    fn as_ref(&self) -> &TreeNodeBase {
        &self.node_base_
    }
}

/// 用户 Scope 在 TreeNodeBase 上的扩展字段
pub(crate) struct ScopeNodeExt {
    flags_: AtomicUsize,

    /// 同父下的兄弟双向链
    prev_sibling_: Option<NonNull<ScopeNode>>,
    next_sibling_: Option<NonNull<ScopeNode>>,

    child_head_: Option<NonNull<ScopeNode>>,
    child_tail_: Option<NonNull<ScopeNode>>,

    /// 指向 Scope 所持有的第一块内存池
    strong_head_: Option<NonNull<StrongPool>>,
    /// 指向 Scope 所持有的最新一块内存池
    strong_tail_: Option<NonNull<StrongPool>>,

    /// 存活链链头（弱槽位）
    live_head_: Option<NonNull<WeakChunk<()>>>,
    /// 存活链链尾（弱槽位）
    live_tail_: Option<NonNull<WeakChunk<()>>>,

    /// 指向 root 中内联的具体分配器；既用于分配，也用于反推 [`RootNodeExt`] 的基址。
    allocator_: &'static dyn Allocator,
}

impl ScopeNode {
    /// 只统计最新的一个内存链中可分配空间大小，因为其他空间默认不会被提前释放，
    /// 因此不必统计。`Scope` 或者说所有 Arena 的使用者就是为了一次性兜底释放，
    /// 才会选用 Arena 而不是直接用智能指针。
    pub fn free_size(&self) -> usize {
        let Option::Some(f) = self.scope_ext_.strong_tail_ else {
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
        if let Option::Some(tail) = self.scope_ext_.strong_tail_ {
            // SAFETY: 池链上的池都由本域分配器分配且仍然存活
            let pool = unsafe { tail.as_ptr().as_mut() }.ok_or(AllocError)?;
            if let Result::Ok(mem) = pool.allocate_(layout) {
                return Result::Ok(mem);
            }
        }
        // 需要新池：容量取"够放下这一块"与默认页大小的较大者
        let need = (StrongPool::THIS_SIZE + layout.size()).div_ceil(DEFAULT_CELL_SIZE) + 1;
        let cell_count = need.max(DEFAULT_PAGE_SIZE / DEFAULT_CELL_SIZE);
        if cell_count > PoolIndex::MAX as usize {
            return Result::Err(AllocError);
        }
        let pool = StrongPool::try_new_(
            cell_count as PoolIndex,
            self.scope_ext_.allocator_,
            self.scope_ext_.strong_tail_,
        )?;
        if self.scope_ext_.strong_head_.is_none() {
            self.scope_ext_.strong_head_ = Option::Some(pool);
        }
        self.scope_ext_.strong_tail_ = Option::Some(pool);
        // SAFETY: pool 是刚初始化的独占池
        unsafe { pool.as_ptr().as_mut() }
            .ok_or(AllocError)?
            .allocate_(layout)
            .map_err(|_| AllocError)
    }
}

impl ScopeNodeExt {
    pub fn allocator(&self) -> &dyn Allocator {
        self.allocator_
    }
}

impl AsRef<ScopeNodeExt> for ScopeNode {
    fn as_ref(&self) -> &ScopeNodeExt {
        &self.scope_ext_
    }
}
