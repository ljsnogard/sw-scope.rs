//! `Sharing<'a, T, M = Local>`：类似 `Arc<T>` 的 scope 引用计数共享句柄。
//!
//! 当前只为 `M = Local` 实现，并保持 `!Send + !Sync`。

use core::{marker::PhantomData, ptr::NonNull};

use crate::{
    abs_::TrShareMarker,
    share_marker_::Local,
    strong_::StrongChunk,
    weak_::DataState,
};

use super::retain_::claim_and_destroy_if_unreachable_;

/// Like Arc<T> but scoped.
pub struct Sharing<'a, T, M = Local>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    strong_chunk_: NonNull<StrongChunk<T>>,
    _lifetime_a_: PhantomData<&'a T>,
    _mode_: PhantomData<M>,
}

impl<'a, T> Sharing<'a, T, Local>
where
    T: 'a + ?Sized,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
        Sharing {
            strong_chunk_: chunk,
            _lifetime_a_: PhantomData,
            _mode_: PhantomData,
        }
    }
}

impl<'a, T> core::ops::Deref for Sharing<'a, T, Local>
where
    T: 'a + ?Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        debug_assert!(chunk.is_data_alive());
        chunk.try_get_data().expect("[Owning::deref] should always work.")
    }
}

impl<'a, T, U> core::ops::CoerceUnsized<Sharing<'a, U, Local>> for Sharing<'a, T, Local>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{}

impl<'a, T> Clone for Sharing<'a, T, Local>
where
    T: 'a + ?Sized,
{
    /// 复制一个共享句柄：只把强计数加 1。
    fn clone(&self) -> Self {
        // SAFETY: Sharing 持有强块指针，强块活着时它有效
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        // SAFETY: 同上；强块活着时它的身份槽位也活着
        let weak = unsafe { chunk.weak_chunk().as_ref() };
        // 克隆必须与“最后一个 Sharing 归零并改回 Created”的临界区互斥，否则可能把
        // 一个正在回退的状态重新加回 Sharing，造成状态与强计数不一致。
        let guard = weak.lock_busy();
        debug_assert_eq!(
            guard.data_state(),
            DataState::Sharing,
            "Sharing 句柄存在时状态必须是 Sharing"
        );
        chunk.incr_strong_count();
        drop(guard);
        Sharing {
            strong_chunk_: self.strong_chunk_,
            _lifetime_a_: PhantomData,
            _mode_: PhantomData,
        }
    }
}

impl<'a, T, M> Drop for Sharing<'a, T, M>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    /// 释放一个共享句柄：强计数减 1。
    ///
    /// **归零不析构数据**，只把状态还给 `Created`（`Retain` 还在，数据仍应活着）；
    /// 只有"强计数归零且弱计数也为 0"这种组合才顺带走确定性析构。
    fn drop(&mut self) {
        // SAFETY: 同上
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        // SAFETY: 同上；强块活着时身份槽位也活着
        let weak = unsafe { chunk.weak_chunk().as_ref() };
        // 强计数减到 0 与状态 Sharing -> Created 必须在同一个临界区内完成；否则并发的
        // `try_sharing` / `clone` 可能在本句柄减到 0 后、状态尚未回退前又加回计数。
        let guard = weak.lock_busy();
        if chunk.decr_strong_count() != 0 {
            return;
        }
        let _ = guard
            .chunk_state()
            .try_transition_state(DataState::Sharing, DataState::Created);
        drop(guard);
        let _ = claim_and_destroy_if_unreachable_(weak);
    }
}
