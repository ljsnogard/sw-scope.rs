//! 域的延迟清盘：`Scope::close` 之后，在没有直接数据视图时强制析构全部存活对象。
//!
//! 这里不处理父子域，也不依赖旧的存活链字段。存活槽位由弱池链 + 槽位状态直接枚举；
//! 析构入口复用 `index_` 的 `force_destroy_slot_`，因此与“最后一个 Retain 释放”的
//! 确定性析构共用同一套 vtable 和 `try_claim_destroy` 语义。

use core::ptr;

use crate::{index_::force_destroy_slot_, strong_::StrongPool, weak_::DataState};

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
        let mut all_dead = true;
        let mut pool = self.weak_head_;
        while let Option::Some(pool_ptr) = pool {
            // SAFETY: 弱池链上的池都由本域分配且仍然存活
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
    }

    /// 整链回收本域的强池。
    ///
    /// # Safety
    ///
    /// 本域所有强块数据必须已经析构，且不再有代码会通过 `WeakSlot::strong_chunk()` 解引用
    /// 这些池。
    unsafe fn reclaim_strong_pools_(&mut self) {
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
