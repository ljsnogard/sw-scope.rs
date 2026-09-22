//! `Retain<T, M = Local>`：不带生命周期的 GC Handle，也是对象存活计数与升级入口。
//!
//! 当前只为 `M = Local` 实现方法，且保持 `!Send + !Sync`。

use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
};

use crate::{
    abs_::TrShareMarker,
    share_marker_::Local,
    strong_::StrongChunk,
    weak_::{DataState, WeakChunk},
};

use super::{owning_::Owning, sharing_::Sharing};

/// Like a gc handle. The lifetime is the same as the scope that allocates it.
pub struct Retain<T, M = Local>
where
    T: ?Sized,
    M: TrShareMarker,
{
    /// We are guaranteed that `WeakChunk<T>` always outlives everything in
    /// the `Weak<T>`;
    weak_chunk_: NonNull<WeakChunk<T>>,
    _mode_: PhantomData<M>,
}

impl<T> Retain<T, Local>
where
    T: ?Sized,
{
    /// 尝试从 Weak<T> 提升为 Retain<T>。
    /// 当且仅当 `DataState::Allocated` 时会成功
    pub fn try_owning(&self) -> Option<Owning<'_, T, Local>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        let strong = chunk.try_owning().ok()?;
        Option::Some(Owning::new(raw_to_strong_::<T>(strong)))
    }

    /// 尝试从 Weak<T> 提升为 Shared<T>。
    /// 当且仅当 `DataState::Allocated` 时会成功。
    pub fn try_sharing(&self) -> Option<Sharing<'_, T, Local>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        let strong = chunk.try_sharing().ok()?;
        Option::Some(Sharing::new(raw_to_strong_::<T>(strong)))
    }

    /// 判断该句柄是否已经进入**僵尸状态**。
    ///
    /// 僵尸状态表示：对象数据已经由 teardown 析构，但这个 `Retain` 仍然存活。
    /// 此时：
    ///
    /// - `try_owning()` / `try_sharing()` 一定失败；
    /// - 句柄本身仍可安全 `clone` / `drop`；
    /// - 最后一个僵尸 `Retain` 析构时，会把这个 `WeakChunk` 归还给 `WeakPool`。
    ///
    /// # Examples
    ///
    /// ```
    /// use sw_scope::{Scope, TrScope};
    ///
    /// let mut scope = Scope::new();
    /// let retain = scope.put(1u64);
    /// assert!(!retain.is_zombie());
    /// ```
    pub fn is_zombie(&self) -> bool {
        // SAFETY: Retain 自己保证 WeakChunk 活着
        let weak = unsafe { self.weak_chunk_.cast::<WeakChunk<()>>().as_ref() };
        weak.is_zombie_()
    }
    /// 构造一个句柄。分配管线（Batch 2）落地前，生产路径还没有调用点。
    #[allow(dead_code)]
    pub(crate) const fn new(chunk: NonNull<WeakChunk<T>>) -> Self {
        Retain {
            weak_chunk_: chunk,
            _mode_: PhantomData,
        }
    }
}

impl<T> Clone for Retain<T, Local>
where
    T: ?Sized,
{
    /// 复制一个句柄：弱计数加 1。
    fn clone(&self) -> Self {
        // SAFETY: Retain 自己保证 WeakChunk 活着
        let weak = unsafe { self.weak_chunk_.as_ref() };
        weak.incr_weak_count();
        Retain {
            weak_chunk_: self.weak_chunk_,
            _mode_: PhantomData,
        }
    }
}

impl<T, M> Drop for Retain<T, M>
where
    T: ?Sized,
    M: TrShareMarker,
{
    /// 释放一个句柄：弱计数减 1；减到 0 时尝试走"确定性析构"。
    ///
    /// 如果 teardown 已经先一步把数据析构、并因为还有逃逸 `Retain` 而把槽位推进到
    /// Zombie 状态，则最后一个逃逸 `Retain` 负责把 `WeakChunk` 归还给 `WeakPool`。
    fn drop(&mut self) {
        // SAFETY: Retain 自己保证 WeakChunk 活着；T 只影响类型，不影响槽位布局
        let weak = unsafe { self.weak_chunk_.cast::<WeakChunk<()>>().as_ref() };
        let state_before = weak.data_state();
        if weak.decr_weak_count() != 0 {
            return;
        }

        if state_before == DataState::Zombie {
            // SAFETY: weak_count 刚从 1 减到 0，且状态是 Zombie
            unsafe { weak.try_reclaim_zombie_() };
            return;
        }

        // 弱计数归零：若此刻没有强引用（状态为 Created），立即走来源 (a)。
        // 若状态仍是 Owning / Sharing（只可能来自泄漏或 unsafe），留给清盘。
        let claimed = claim_and_destroy_if_unreachable_(weak);
        // 竞争兜底：teardown 可能在本线程读完 state_before 之后才把状态推到 Zombie；
        // 若我们的 claim 没成功，而当前已经是 Zombie，则接手归还槽位。
        if !claimed && weak.data_state() == DataState::Zombie {
            // SAFETY: weak_count 已经归零，状态是 Zombie
            unsafe { weak.try_reclaim_zombie_() };
        }
    }
}
/// 走"确定性析构"来源 (a)：当且仅当"弱计数为 0 且状态为 `Created`"时认领并执行清理。
///
/// 这是三处引用计数收尾（`Owning::drop` / `Sharing::drop` / `Retain::drop`）共用的判断，
/// 保证"恰好销毁一次"只由 `try_claim_destroy` 的 CAS 裁决。返回本次是否完成了析构。
pub(super) fn claim_and_destroy_if_unreachable_(weak: &WeakChunk<()>) -> bool {
    if weak.weak_count() != 0 || weak.data_state() != DataState::Created {
        return false;
    }
    if weak.chunk_state().try_claim_destroy().is_none() {
        return false;
    }
    // SAFETY: 刚由本调用认领成功，且本路径恰好执行一次
    unsafe { weak.drop_data() };
    weak.chunk_state().mark_destroyed();
    true
}

/// 把"类型擦除后的强块头部指针"还原成 `NonNull<StrongChunk<T>>`。
///
/// 对 `?Sized` 的 `T` 需要重建胖指针，元数据取自弱槽位的析构登记（弱槽位是生命周期
/// 最长的一环，见 `WeakChunk` 的文档）。
fn raw_to_strong_<T: ?Sized + ptr::Pointee>(
    raw: NonNull<crate::strong_::StrongChunkBase>,
) -> NonNull<StrongChunk<T>> {
    let void = raw.as_ptr().cast::<()>();
    let meta = raw_meta_::<T>(raw);
    // SAFETY: 由调用方保证该强块当初就是按 T 分配的；头部在偏移 0，因此同址
    unsafe { NonNull::new_unchecked(ptr::from_raw_parts_mut::<StrongChunk<T>>(void, meta)) }
}

/// 从强块头部取出它当初分配时的类型元数据。
fn raw_meta_<T: ?Sized + ptr::Pointee>(
    raw: NonNull<crate::strong_::StrongChunkBase>,
) -> T::Metadata {
    // SAFETY: 强块头部记录着身份槽位，槽位必然比强块活得久
    let weak = unsafe { raw.as_ref().weak_chunk() };
    // SAFETY: 同上
    let weak = unsafe { weak.as_ref() };
    #[cfg(test)]
    {
        let _ = weak;
    }
    // SAFETY: 登记信息是用同一个 T 写下的
    unsafe { crate::weak_::meta_from_raw_::<T>(crate::weak_::meta_of_(weak)) }
}
