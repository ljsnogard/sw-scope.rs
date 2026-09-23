//! `Owning<'a, T, M = Local>`：类似 `Box<T>` 的 scope 独占访问句柄。
//!
//! 当前只为 `M = Local` 实现；它只归还访问权，不负责析构数据。

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

    pub fn retained(&self) -> Retain<T, M> {
        todo!()
    }
}

impl<'a, T> Owning<'a, T, Local>
where
    T: 'a + Sized,
{
    pub fn try_new_local(
        data: T,
        scope: &'a mut Scope<Local>,
    ) -> Result<Self, ScopeError> {
        todo!()
    }
}

impl<'a> Owning<'a, ScopeStr, Local> {
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
