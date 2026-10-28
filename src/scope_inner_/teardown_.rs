//! 域的延迟清盘与池回收。
//!
//! `Scope::close` 之后，在没有直接数据视图时强制析构全部存活对象；root 最终清理时，
//! 则直接回收实体池里剩余的弱池 / 强池（此时所有域都已登记退出）。
//!
//! # 并发要点
//!
//! `flush_data_` 持有域写锁遍历弱池链，同时可能**级联析构**数据里的 `Retain`。这些
//! `Retain` 的 drop 会进入末槽归还路径，而归还路径可能想摘链并释放空池。为了避免
//! “遍历中的池被释放”，清盘分三步：
//!
//! 1. 在弱池链锁内**钉住**链上所有池（`WeakPool::pin_`）；
//! 2. 释放链锁后遍历 / 析构；此时归还者只能把空池标记为 `retire_pending_`；
//! 3. 重新拿链锁逐个解钉，把已经在钉住期间变空的池摘下，最后在锁外释放内存。
//!
//! 与 [`crate::weak_::WeakPool`] 的锁顺序约定一致：**域写锁 → 弱池链锁 → 池锁**。

use core::ptr;

use crate::{
    index_::force_destroy_slot_,
    strong_::StrongPool,
    weak_::{DataState, WeakPool},
};

use super::ScopeInner;

impl ScopeInner {
    /// 强制析构本域所有仍然存活的数据。
    ///
    /// 外部 `Retain` 可以存在，但只由 `Retain` 持有的对象会在本轮被清除；清盘后这些
    /// `Retain` 只会看到 `Destroyed` 状态，`try_owning` / `try_sharing` 会失败。
    /// `Owning` / `Sharing` 指向的对象本轮跳过并保留，等视图释放后再清盘。
    ///
    /// 数据析构可能通过字段里的 `Retain` 级联析构别的对象；这些对象随后在本循环里被
    /// `try_claim_destroy` 判为已处理。所有槽位都死亡后才整链回收强池。
    ///
    /// # Safety
    ///
    /// 必须在持有本域写锁（或等价地确认没有并发放入）时调用。
    pub(super) unsafe fn flush_data_(&mut self) {
        // 1) 钉住整条弱池链：阻止并发归还者摘链 / 释放池内存。
        self.pin_weak_chain_();
        let mut all_dead = true;
        let mut pool = self.weak_head_();
        while let Option::Some(pool_ptr) = pool {
            // SAFETY: 弱池链上的池都由本域分配、钉住期间不会被释放
            let pool_ref = unsafe { pool_ptr.as_ref() };
            pool = pool_ref.next();
            let capacity = pool_ref.capacity();
            for index in 0..capacity {
                // SAFETY: index < capacity，槽位落在池内
                let slot = unsafe { pool_ref.slot_at_(index) };
                match slot.data_state() {
                    DataState::Reclaimed => continue,
                    // 只有“无直接视图”的 Created 对象可以强制析构；Owning / Sharing 必须跳过。
                    DataState::Created => {
                        // SAFETY: 本域已请求关闭，且该槽位当前没有直接数据视图；认领后
                        // 恰好析构一次
                        let _ = unsafe { force_destroy_slot_(slot) };
                    }
                    // 直接视图仍然存活，或另一个线程正在析构：本轮不能回收强池。
                    DataState::Owning | DataState::Sharing | DataState::Destroying => {
                        all_dead = false;
                        continue;
                    }
                    DataState::Destroyed | DataState::Finalized => {}
                }
                if slot.data_state().is_data_alive() {
                    all_dead = false;
                } else {
                    // 数据已销毁，断开槽位到强块的指针，避免外部 Retain 顺着它访问已回收内存。
                    slot.set_strong_chunk(ptr::null_mut());
                }
            }
        }
        if all_dead {
            // SAFETY: 所有槽位都已不再存活；外部 Retain 只读弱槽位状态
            unsafe { self.reclaim_strong_pools_() };
        }
        // 2) 解钉，并释放钉住期间变空的池。
        self.unpin_weak_chain_();
    }

    /// 钉住链上所有弱池；调用方必须持有域写锁。
    fn pin_weak_chain_(&self) {
        self.with_weak_chain_(|chain| {
            let mut cursor = chain.head_;
            while let Option::Some(pool) = cursor {
                // SAFETY: 链锁保证链上的池仍然存活
                let pool_ref = unsafe { pool.as_ref() };
                cursor = pool_ref.next();
                pool_ref.pin_();
            }
        });
    }

    /// 解除钉住；把钉住期间变空的池摘下，并在链锁之外释放。
    fn unpin_weak_chain_(&mut self) {
        let mut pending_free = Option::None;
        self.with_weak_chain_(|chain| {
            let mut cursor = chain.head_;
            while let Option::Some(pool) = cursor {
                // SAFETY: 链锁保证链上的池仍然存活
                let pool_ref = unsafe { pool.as_ref() };
                let next = pool_ref.next();
                let unpinned = pool_ref.unpin_();
                if unpinned && pool_ref.try_claim_after_unpin_() {
                    chain.unlink_(pool);
                    // 复用池自己的 next_ 字段串一条临时待释放链；此时它已经不在弱池链上。
                    // SAFETY: 池已 detached，不再被其它线程访问
                    let Option::Some(pool_mut) = (unsafe { pool.as_ptr().as_mut() }) else {
                        cursor = next;
                        continue;
                    };
                    pool_mut.set_next_(pending_free);
                    pending_free = Option::Some(pool);
                }
                cursor = next;
            }
        });
        let alloc = unsafe { self.allocator_.as_ref() };
        let mut cursor = pending_free;
        while let Option::Some(pool) = cursor {
            // SAFETY: 待释放链上的池都已摘链且不再被访问
            let next = unsafe { pool.as_ref() }.next();
            // SAFETY: 同上
            unsafe { WeakPool::dealloc_(pool, alloc) };
            cursor = next;
        }
    }

    /// 释放本域弱池链上的**全部**弱池，不管其活槽计数。
    ///
    /// 只用于 root 最终清理：调用时所有域都已登记退出，实体池里剩下的弱池只承载实体
    /// 自身的身份槽位，不会再被任何 `Retain` 使用。
    ///
    /// # Safety
    ///
    /// 调用方必须独占本 `ScopeInner`（持有 root 实体池锁），且不存在任何仍在使用的
    /// `WeakSlot` / `WeakPool` 指针。
    pub(crate) unsafe fn free_all_weak_pools_(&mut self) {
        let head = self.with_weak_chain_(|chain| chain.take_all_());
        let alloc = unsafe { self.allocator_.as_ref() };
        let mut cursor = head;
        while let Option::Some(pool) = cursor {
            // SAFETY: 调用方保证没有其它线程访问链上的池
            let next = unsafe { pool.as_ref() }.next();
            // SAFETY: 同上
            unsafe { WeakPool::dealloc_(pool, alloc) };
            cursor = next;
        }
    }

    /// 整链回收本域的强池。
    ///
    /// # Safety
    ///
    /// 本域所有强块数据必须已经析构，且不再有代码会通过 `WeakSlot::strong_chunk()` 解引用
    /// 这些池。
    pub(crate) unsafe fn reclaim_strong_pools_(&mut self) {
        let alloc = unsafe { self.allocator_.as_ref() };
        let mut cursor = self.strong_tail_;
        while let Option::Some(pool) = cursor {
            // SAFETY: 池链上的池都由本域分配器按 layout_for_ 分配且仍然存活
            let pool_ref = unsafe { pool.as_ref() };
            cursor = pool_ref.prev_();
            unsafe {
                alloc.deallocate(
                    pool.cast::<u8>(),
                    StrongPool::layout_for_(pool_ref.cell_count_()),
                );
            }
        }
        self.strong_head_ = Option::None;
        self.strong_tail_ = Option::None;
    }
}
