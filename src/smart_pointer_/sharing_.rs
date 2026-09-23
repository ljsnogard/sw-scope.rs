//! `Sharing<'a, T, M = Local>`：类似 `Arc<T>` 的 scope 引用计数共享句柄。
//!
//! 与 `Owning` 一样，它既可由 `Scope` 直接构造，也可由 [`Retain::try_sharing`] 升级得到。
//! 尚未关联 `WeakChunk` 时，强计数归零即析构数据（同 `Arc`）；已关联 `WeakChunk` 时，强计数
//! 归零只归还访问权，数据由 `Retain` / 清盘负责。
//!
//! 当前只为 `M = Local` 实现，并保持 `!Send + !Sync`。

use core::{marker::PhantomData, ptr::NonNull};

use crate::{
    abs_::TrShareMarker,
    share_marker_::Local,
    strong_::StrongChunk,
    weak_::DataState,
};

use super::retain_::{Retain, claim_and_destroy_if_unreachable_};

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

impl<'a, T, M> Sharing<'a, T, M>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
        Sharing {
            strong_chunk_: chunk,
            _lifetime_a_: PhantomData,
            _mode_: PhantomData,
        }
    }

    /// 取得（必要时补建）该对象的 [`Retain`] 身份句柄。
    ///
    /// 语义同 [`crate::Owning::retained`]：首次调用补建 `WeakChunk`，此后该槽位在强块的整个
    /// 生命周期内一直存在。拿到 `Retain` 后，强计数归零不再当场析构数据。
    pub fn retained(&self) -> Retain<T, M> {
        todo!()
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
        chunk.incr_strong_count();
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
    /// 释放一个共享句柄：强计数减 1。若本块没有关联 `WeakChunk`，最后一个句柄析构时行为
    /// 同 `Arc` / `Rc`，当场析构数据。
    ///
    /// 若已关联 `WeakChunk`，则 **归零不析构数据**，只把访问权还回 `Retained`（`Retain` 还在，
    /// 数据仍应活着）；只有"强计数归零且弱计数也为 0"这种组合才顺带走确定性析构。
    fn drop(&mut self) {
        todo!()
    }
}
