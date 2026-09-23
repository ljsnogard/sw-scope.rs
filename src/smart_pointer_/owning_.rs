//! `Owning<'a, T, M = Local>`：类似 `Box<T>` 的 scope 独占访问句柄。
//!
//! 它有两条产生路径：
//!
//! - `Scope` 直接构造（[`Owning::try_new_local`] / [`Owning::try_local_str`]）：此时对象还
//!   没有 `WeakChunk`，`Owning` 就是唯一负责人，析构行为同 `Box<T>`；
//! - [`Retain::try_owning`]：此时对象已经关联 `WeakChunk`，析构只归还访问权，数据由
//!   `Retain` / 清盘负责。
//!
//! 当前只为 `M = Local` 实现。

use core::{marker::PhantomData, ptr::NonNull};

use crate::{
    abs_::TrShareMarker,
    scope_::{Scope, ScopeError},
    scope_str_::ScopeStr,
    share_marker_::Local,
    strong_::StrongChunk,
    weak_::DataState,
};

use super::retain_::Retain;

/// Like Box<T> but scoped.
#[repr(transparent)]
pub struct Owning<'a, T, M = Local>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    strong_chunk_: NonNull<StrongChunk<T>>,
    _unused_lt_a_: PhantomData<&'a T>,
    _using_local_: PhantomData<M>,
}

impl<'a, T, M> Owning<'a, T, M>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
        Owning {
            strong_chunk_: chunk,
            _unused_lt_a_: PhantomData,
            _using_local_: PhantomData,
        }
    }

    pub fn data_state(&self) -> DataState {
        todo!()
    }

    /// 取得（必要时补建）该对象的 [`Retain`] 身份句柄。
    ///
    /// 若此前从未取得过 `Retain`，本调用会为强块补建一个 `WeakChunk` 身份槽位；此后该槽位
    /// 在强块的整个生命周期内一直存在，不会消失也不会改指。拿到 `Retain` 后，对象的析构
    /// 改由 `Retain` + 状态机决定（强计数归零不再当场析构）。
    pub fn retained(&self) -> Retain<T, M> {
        todo!()
    }
}

impl<'a, T> Owning<'a, T, Local>
where
    T: 'a + Sized,
{
    /// 直接把 `data` 放进 `scope`，返回独占句柄。
    ///
    /// 这是"不必先有 `Retain` 也能把对象放进 Scope"的入口：此时对象没有 `WeakChunk`，
    /// 句柄析构即就地析构数据（同 `Box<T>`）。之后若调用 [`Owning::retained`]，才会补建
    /// 身份槽位并转入 `Retain` 路径。
    pub fn try_new_local(
        data: T,
        scope: &'a mut Scope<Local>,
    ) -> Result<Self, ScopeError> {
        todo!()
    }
}

impl<'a> Owning<'a, ScopeStr, Local> {
    /// [`Owning::try_new_local`] 的 `str` 版本：把 `str` 拷贝进 `scope`。
    pub fn try_local_str(
        str: &str,
        scope: &'a mut Scope<Local>,
    ) -> Result<Self, ScopeError> {
        todo!()
    }
}

impl<'a, T> core::ops::Deref for Owning<'a, T, Local>
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

impl<'a, T> core::ops::DerefMut for Owning<'a, T, Local>
where
    T: 'a + ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        let chunk = unsafe { self.strong_chunk_.as_mut() };
        debug_assert!(chunk.is_data_alive());
        chunk
            .try_get_data_mut()
            .expect("[Owning::deref_mut] should always work.")
    }
}

impl<'a, T, U> core::ops::CoerceUnsized<Owning<'a, U, Local>> for Owning<'a, T, Local>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{}

impl<'a, T, M> Drop for Owning<'a, T, M>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    /// `Owning` 析构时，如果 StrongChunk 没有指向 WeakChunk，行为同 Box<T>。
    /// 否则只归还数据访问权，不会马上析构资源。
    fn drop(&mut self) {
        todo!()
    }
}

unsafe impl<T, M> Send for Owning<'_, T, M>
where
    T: Send + ?Sized,
    M: Send + TrShareMarker,
{}

unsafe impl<T, M> Sync for Owning<'_, T, M>
where
    T: Sync + ?Sized,
    M: Send + Sync + TrShareMarker,
{}
