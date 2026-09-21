use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
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
        chunk.try_get_data().expect("[Owning::deref] should always work.")
    }
}

impl<'a, T> core::ops::DerefMut for Owning<'a, T>
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
        chunk.try_get_data().expect("[Owning::deref] should always work.")
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
        let strong = chunk.try_owning().ok()?;
        Option::Some(Owning::new(raw_to_strong_::<T>(strong)))
    }

    /// 尝试从 Weak<T> 提升为 Shared<T>。
    /// 当且仅当 `DataState::Allocated` 时会成功。
    pub fn try_sharing(&self) -> Option<Sharing<'_, T>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        let strong = chunk.try_sharing().ok()?;
        Option::Some(Sharing::new(raw_to_strong_::<T>(strong)))
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
