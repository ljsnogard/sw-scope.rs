use core::{
    ptr::NonNull,
    marker::PhantomData,
};

use crate::{
    strong_::StrongChunk,
    weak_::WeakChunk,
};

/// Like Box<T> but scoped.
pub struct Owning<'a, T>
where
    T: 'a + ?Sized,
{
    strong_chunk_: NonNull<StrongChunk<T>>,
    _lifetime_a_: PhantomData<&'a T>,
}

/// Like Arc<T> but scoped.
pub struct Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    strong_chunk_: NonNull<StrongChunk<T>>,
    _lifetime_a_: PhantomData<&'a T>,
}

/// Like a gc handle. The lifetime is the same as the scope that allocates it.
pub struct Retain<T>
where
    T: ?Sized,
{
    /// We are guaranteed that `WeakChunk<T>` always outlives everything in
    /// the `Weak<T>`;
    weak_chunk_: NonNull<WeakChunk<T>>,
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<'a, T> Owning<'a, T>
where
    T: 'a + ?Sized,
{
    const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
        Owning {
            strong_chunk_: chunk,
            _lifetime_a_: PhantomData,
        }
    }
}

impl<'a, T> core::ops::Deref for Owning<'a, T>
where
    T: 'a + ?Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        debug_assert!(chunk.is_data_alive());
        chunk.try_get_data().expect("[Retain::deref] should always work.")
    }
}

impl<'a, T> core::ops::DerefMut for Owning<'a, T>
where
    T: 'a + ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        let chunk = unsafe { self.strong_chunk_.as_mut() };
        debug_assert!(chunk.is_data_alive());
        chunk.try_get_data_mut().expect("[Retain::deref_mut] should always work.")
    }
}

impl<'a, T, U> core::ops::CoerceUnsized<Owning<'a, U>> for Owning<'a, T>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<'a, T> Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
        Sharing {
            strong_chunk_: chunk,
            _lifetime_a_: PhantomData,
        }
    }
}

impl<'a, T> core::ops::Deref for Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        debug_assert!(chunk.is_data_alive());
        chunk.try_get_data().expect("[Retain::deref] should always work.")
    }
}

impl<'a, T, U> core::ops::CoerceUnsized<Sharing<'a, U>> for Sharing<'a, T>
where
    T: ?Sized + core::marker::Unsize<U>,
    U: ?Sized,
{}

impl<'a, T> Clone for Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    fn clone(&self) -> Self {
        todo!("increase strong count")
    }
}

impl<'a, T> Drop for Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    fn drop(&mut self) {
        todo!()
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> Retain<T>
where
    T: ?Sized,
{
    pub(crate) const fn new(chunk: NonNull<WeakChunk<T>>) -> Self {
        Retain { weak_chunk_: chunk }
    }

    /// 尝试从 Weak<T> 提升为 Retain<T>。
    /// 当且仅当 `DataState::Allocated` 时会成功
    pub fn try_owning(&self) -> Option<Owning<'_, T>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        chunk.try_retain()
            .map(Owning::new)
            .ok()
    }

    /// 尝试从 Weak<T> 提升为 Shared<T>。
    /// 当且仅当 `DataState::Allocated` 时会成功。
    pub fn try_sharing(&self) -> Option<Sharing<'_, T>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        chunk.try_share()
            .map(Sharing::new)
            .ok()
    }
}

impl<T> Clone for Retain<T>
where
    T: ?Sized,
{
    fn clone(&self) -> Self {
        todo!("increase weak count")
    }
}

impl<T> Drop for Retain<T>
where
    T: ?Sized,
{
    fn drop(&mut self) {
        todo!()
    }
}
