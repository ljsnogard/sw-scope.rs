//! 清盘方案 A：关闭子树、静默判定、待回收名单与整域回收。
//!
//! 这里的方法紧密共享存活链、强池链、待回收名单这组字段，因此放在同一个文件里。

use core::{alloc::Layout, ptr::NonNull, sync::atomic::Ordering};

use crate::{strong_::StrongPool, weak_::{DataState, WeakChunk, WeakPool}};

use super::{PoolIndex, RootExtension, ScopeInner};

impl<const CELL_SIZE: usize> ScopeInner<CELL_SIZE> {
    /// 整块回收强池链：把每个池的内存还给分配器。
    ///
    /// 只在域静默（没有存活对象、也没有子域）时调用；调用方负责在此之前完成 `PreDrop`
    /// 与数据析构。
    pub(crate) fn reclaim_strong_pools_(&mut self) {
        let alloc = self.allocator_;
        let mut cursor = self.chain_tail_;
        while let Option::Some(pool) = cursor {
            // SAFETY: 池仍由本域持有
            let pool_ref = unsafe { pool.as_ref() };
            cursor = pool_ref.prev_();
            // SAFETY: 池是按 `layout_for_` 分配的，且此时仍未被回收
            unsafe {
                alloc.deallocate(
                    pool.cast::<u8>(),
                    StrongPool::<CELL_SIZE>::layout_for_(pool_ref.cell_count_()),
                )
            };
        }
        self.chain_head_ = Option::None;
        self.chain_tail_ = Option::None;
    }

    /// 清盘的保证路径（来源 (b)）：遍历存活链，把**仍然活着**的对象逐个 `PreDrop` + 析构，
    /// 把槽位还回所属弱池，然后整块回收强池。
    ///
    /// 与来源 (a) 共用同一个 `try_claim_destroy` 认领 CAS：级联析构或并发 `drop` 已经处理过
    /// 的槽位会认领失败，这里直接跳过，因此"恰好一次"仍然成立。
    ///
    /// # 为什么 `weak_count` 不妨碍清盘
    ///
    /// 清盘由 `Scope` 发起，是"保证路径"：环、`mem::forget`、泄漏句柄都不能阻止数据被
    /// `PreDrop` + `drop_in_place`。因此这里不能因为 `weak_count > 0` 就跳过析构。
    ///
    /// 但 `weak_count` 决定**弱槽位能不能回收复用**：只要还有 `Retain` 活着，它仍可能
    /// 在之后的任意时刻调用 `try_owning` / `try_sharing`。若此时把槽位还给 `WeakPool`，
    /// 后续新对象可能复用同一块内存，旧 `Retain` 就会变成指向新对象的悬空 / 别名句柄。
    ///
    /// 所以规则是：
    /// - 数据析构：照常执行；
    /// - 槽位回收：必须等 `weak_count == 0` 再做；否则保留槽位，进入不再复用的
    ///   "zombie handle" 状态，逃逸的 `Retain` 后续只能失败。
    ///
    /// # Safety
    ///
    /// 调用后本域中所有对象的**数据**都会析构；调用方必须保证不通过正常借用继续访问
    /// 数据。逃逸出去的 `Retain` 仍可安全地失败，但不能再认领任何对象。
    pub(crate) unsafe fn flush_(&mut self) {
        let mut cursor = self.live_head_;
        while let Option::Some(weak) = cursor {
            // SAFETY: 链上的槽位都由本域分配且仍然有效
            let weak_ref = unsafe { weak.as_ref() };
            // 先把后继取出来，因为下面可能把本槽位还给池（会清掉链接）
            cursor = NonNull::new(weak_ref.next_live());
            if weak_ref.chunk_state().try_claim_destroy().is_some() {
                // SAFETY: 刚由本调用认领成功，恰好执行一次
                unsafe { weak_ref.drop_data() };
                weak_ref.chunk_state().mark_destroyed();
            }

            if weak_ref.weak_count() == 0 {
                // 没有逃逸的 Retain 了：可以按既有语义终态化并归还槽位。
                if weak_ref.chunk_state().try_finalize_from(DataState::Destroyed) {
                    // 归还槽位：反查所属池 -> deallocate 会把状态、链接与登记一并重置
                    if let Option::Some((pool, index)) = self.weak_pool_of_(weak) {
                        // SAFETY: pool 由本树持有且存活
                        if let Option::Some(pool_ref) = unsafe { pool.as_ptr().as_mut() } {
                            let _ = pool_ref.deallocate(index);
                        }
                    }
                }
            } else {
                // 数据已经析构，但还有 Retain 逃逸在世。此时禁止复用这块弱槽位：
                // - 先登记“最后一个 Retain 消失时如何归还 WeakPool”的入口；
                // - 清掉 live 链接；
                // - 最后用 CAS 发布 Zombie 状态。
                //
                // 发布 Zombie 之后不能再访问 weak_ref：最后一个 Retain 可能已经在
                // 另一个线程里 CAS Finalized 并把槽位归还给 WeakPool。这里接受一个
                // 极小竞争窗口下的安全泄漏（最后一个 Retain 恰在 inspect 与 publish
                // 之间减到 0），但绝不允许复用已被旧句柄看见的槽位。
                weak_ref.set_zombie_reclaimer_(dealloc_zombie_slot_::<CELL_SIZE>);
                weak_ref.set_next_live(core::ptr::null_mut());
                weak_ref.set_prev_live(core::ptr::null_mut());
                let _ = weak_ref.chunk_state().mark_zombie();
            }
        }
        self.live_head_ = Option::None;
        self.live_tail_ = Option::None;
        self.reclaim_strong_pools_();
    }

    /// 本域是否**静默**：存活链上没有任何仍然活着的数据。
    ///
    /// 关闭子树时子域已被逐个摘下并单独入名单，所以这里不用再看 `first_child_`。
    pub(crate) fn is_quiescent_(&self) -> bool {
        let mut cursor = self.live_head_;
        while let Option::Some(weak) = cursor {
            // SAFETY: 链上的槽位都由本域分配且仍然有效
            let weak_ref = unsafe { weak.as_ref() };
            if weak_ref.data_state().is_data_alive() {
                return false;
            }
            cursor = NonNull::new(weak_ref.next_live());
        }
        true
    }

    /// 本域是否可以被真正回收：**句柄已析构 + 已标记关闭 + 静默**。
    pub(crate) fn is_reclaimable_(&self) -> bool {
        let wanted = Self::FLAG_CLOSED | Self::FLAG_HANDLE_DROPPED;
        let flags = self.flags_.load(Ordering::Acquire);
        flags & wanted == wanted && self.is_quiescent_()
    }

    /// 非递归地把以 `self` 为根的子树标记关闭、逐个从父链摘下、挂进待回收名单。
    ///
    /// 顺序是**子先父后、弟先兄后**（后加入的先入名单）：从 `last_child_` 一路下潜到叶子，
    /// 摘掉"尾子节点"后再决定回父还是继续下潜。O(节点数) 时间、O(1) 栈，不递归。
    pub(crate) fn close_subtree_(&mut self) {
        let root = self.root_inner_();
        let mut cur: *mut Self = self;
        loop {
            // 下潜到最"幼"的叶子
            // SAFETY: cur 始终是子树内的有效节点
            while let Option::Some(child) = unsafe { (*cur).last_child_ } {
                cur = child.as_ptr();
            }
            // SAFETY: cur 有效
            let parent = unsafe { (*cur).parent_scope_ };
            match parent {
                Option::None => {
                    // SAFETY: cur 是子树根
                    unsafe {
                        (*cur).flags_.fetch_or(Self::FLAG_CLOSED, Ordering::AcqRel);
                        Self::push_pending_(root, NonNull::new_unchecked(cur));
                    }
                    break;
                }
                Option::Some(parent) => {
                    // SAFETY: cur 与 parent 都有效；摘掉尾子节点
                    unsafe {
                        let prev = (*cur).prev_sibling_;
                        (*parent.as_ptr()).last_child_ = prev;
                        match prev {
                            Option::Some(prev) => (*prev.as_ptr()).next_sibling_ = Option::None,
                            Option::None => (*parent.as_ptr()).first_child_ = Option::None,
                        }
                        (*cur).prev_sibling_ = Option::None;
                        (*cur).next_sibling_ = Option::None;
                        (*cur).parent_scope_ = Option::None;
                        (*cur).flags_.fetch_or(Self::FLAG_CLOSED, Ordering::AcqRel);
                        Self::push_pending_(root, NonNull::new_unchecked(cur));
                    }
                    // 父还有孩子就继续下潜，否则回父（它现在成了叶子）
                    cur = match unsafe { (*parent.as_ptr()).last_child_ } {
                        Option::Some(child) => child.as_ptr(),
                        Option::None => parent.as_ptr(),
                    };
                }
            }
        }
    }

    /// 尝试回收待回收名单里已经可回收的域。可在任意 `put` 之前或 `collect` 时调用。
    pub(crate) fn reclaim_pending_(&mut self) {
        let root = self.root_inner_();
        let mut prev: Option<NonNull<Self>> = Option::None;
        // SAFETY: root 有效；名单上的链接都指向有效节点
        let mut cursor = unsafe { (*root.as_ptr()).pending_head_ };
        while let Option::Some(node) = cursor {
            // SAFETY: node 有效
            let next = unsafe { (*node.as_ptr()).pending_next_ };
            if unsafe { node.as_ref() }.is_reclaimable_() {
                // SAFETY: prev / root / node 都有效；摘下后立刻释放
                unsafe {
                    match prev {
                        Option::Some(prev) => (*prev.as_ptr()).pending_next_ = next,
                        Option::None => (*root.as_ptr()).pending_head_ = next,
                    }
                    if (*root.as_ptr()).pending_tail_ == Option::Some(node) {
                        (*root.as_ptr()).pending_tail_ = prev;
                    }
                    (*node.as_ptr()).pending_next_ = Option::None;
                    Self::destroy_scope_(node);
                }
            } else {
                prev = Option::Some(node);
            }
            cursor = next;
        }
    }

    /// 由槽位反查它所属的弱池与池内序号（O(1)）。
    ///
    /// 走"弱槽位 → 弱池"的反推（[`WeakPool::of_slot_`]），不再沿池链线性查找；随后用
    /// [`WeakPool::index_of_`] 校验槽位确实落在该池的槽位数组内。
    fn weak_pool_of_(
        &self,
        weak: NonNull<WeakChunk<()>>,
    ) -> Option<(
        NonNull<WeakPool<CELL_SIZE, RootExtension<CELL_SIZE>>>,
        PoolIndex,
    )> {
        let pool = WeakPool::<CELL_SIZE, RootExtension<CELL_SIZE>>::of_slot_(weak)?;
        // SAFETY: pool 由槽位地址反推且已通过对齐校验
        let index = unsafe { pool.as_ref() }.index_of_(weak)?;
        Option::Some((pool, index))
    }

    /// 把 `node` 追加到 root 的待回收名单尾。
    ///
    /// # Safety
    ///
    /// `root` 必须是本树的根域，`node` 必须有效且此刻不在名单里。
    unsafe fn push_pending_(root: NonNull<Self>, node: NonNull<Self>) {
        // SAFETY: 由调用方保证两个指针有效
        unsafe {
            (*node.as_ptr()).pending_next_ = Option::None;
            match (*root.as_ptr()).pending_tail_ {
                Option::Some(tail) => (*tail.as_ptr()).pending_next_ = Option::Some(node),
                Option::None => (*root.as_ptr()).pending_head_ = Option::Some(node),
            }
            (*root.as_ptr()).pending_tail_ = Option::Some(node);
        }
    }

    /// 释放一个已经可回收的域：`flush_` 归还弱槽位并回收强池，再把 `ScopeInner` 还给分配器。
    ///
    /// # Safety
    ///
    /// `node` 必须已经静默（没有存活对象），否则会强制析构仍被引用的数据。
    unsafe fn destroy_scope_(mut node: NonNull<Self>) {
        // SAFETY: 由调用方保证静默
        let node_ref = unsafe { node.as_mut() };
        // SAFETY: 已静默，flush_ 不会析构仍被引用的数据
        unsafe { node_ref.flush_() };
        let alloc = node_ref.allocator_;
        // SAFETY: node 由同一个分配器按 `Layout::new::<Self>()` 分配
        unsafe { alloc.deallocate(node.cast::<u8>(), Layout::new::<Self>()) };
    }
}

/// 僵尸弱槽位的归还入口：由 `ScopeInner::flush_` 注册，由最后一个逃逸 `Retain`
/// 在 [`WeakChunk::try_reclaim_zombie_`] 中调用。
///
/// 它会从弱槽位地址反推所属 `WeakPool`，再把槽位放回空闲链。调用后弱槽位可能立刻被
/// 新对象复用，因此调用方不得再访问该槽位。
///
/// # Safety
///
/// 必须只在 `weak_count == 0` 且状态为 [`DataState::Zombie`] 时调用。
unsafe fn dealloc_zombie_slot_<const CELL_SIZE: usize>(weak: NonNull<WeakChunk<()>>) {
    let Option::Some(pool) =
        WeakPool::<CELL_SIZE, RootExtension<CELL_SIZE>>::of_slot_(weak)
    else {
        return;
    };
    // SAFETY: pool 由弱槽位地址反推得到，且已通过对齐校验
    let Option::Some(index) = unsafe { pool.as_ref() }.index_of_(weak) else {
        return;
    };
    // SAFETY: pool 由本树持有且存活；索引也已校验
    let Option::Some(pool_ref) = (unsafe { pool.as_ptr().as_mut() }) else {
        return;
    };
    let _ = pool_ref.deallocate(index);
}
