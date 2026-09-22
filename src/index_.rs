use core::{
    marker::PhantomData,
    ptr::{self, NonNull},
};

use crate::{
    strong_::StrongChunk,
    weak_::{DataState, WeakChunk},
};

#[cfg(test)]
mod tests_;

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
    pub(crate) const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
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

impl<'a, T> Drop for Owning<'a, T>
where
    T: 'a + ?Sized,
{
    /// `Owning` 析构只**归还访问权**，不析构数据。
    ///
    /// 这样 README 的"`Owning` 析构后还能再次 `try_owning`"才成立：只要 `Retain` 还在，
    /// 数据就仍然活着。真正终结数据要等"最后一个 `Retain` 也消失"（确定性来源 (a)）或清盘
    /// （保证来源 (b)），见 `dev-notes/weak-20260922-1135.md` §2。
    fn drop(&mut self) {
        // SAFETY: Owning 持有强块指针，强块活着时身份槽位也活着
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        // SAFETY: 同上
        let weak = unsafe { chunk.weak_chunk().as_ref() };
        debug_assert!(
            weak.weak_count() > 0,
            "Owning 必然借用自某个 Retain，弱计数不该是 0"
        );
        let _ = weak
            .chunk_state_
            .try_transition_state(DataState::Owning, DataState::Created);
        // 正常路径下弱计数 > 0，这里不会触发；保留判断以覆盖泄漏 / unsafe 组合
        let _ = claim_and_destroy_if_unreachable_(weak);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<'a, T> Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
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
    /// 复制一个共享句柄：只把强计数加 1。
    fn clone(&self) -> Self {
        // SAFETY: Sharing 持有强块指针，强块活着时它有效
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        chunk.incr_strong_count();
        Sharing {
            strong_chunk_: self.strong_chunk_,
            _lifetime_a_: PhantomData,
        }
    }
}

impl<'a, T> Drop for Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    /// 释放一个共享句柄：强计数减 1。
    ///
    /// **归零不析构数据**，只把状态还给 `Created`（`Retain` 还在，数据仍应活着）；
    /// 只有"强计数归零且弱计数也为 0"这种组合才顺带走确定性析构。
    fn drop(&mut self) {
        // SAFETY: 同上
        let chunk = unsafe { self.strong_chunk_.as_ref() };
        if chunk.decr_strong_count() != 0 {
            return;
        }
        // SAFETY: 同上
        let weak = unsafe { chunk.weak_chunk().as_ref() };
        let _ = weak
            .chunk_state_
            .try_transition_state(DataState::Sharing, DataState::Created);
        let _ = claim_and_destroy_if_unreachable_(weak);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> Retain<T>
where
    T: ?Sized,
{
    /// 构造一个句柄。分配管线（Batch 2）落地前，生产路径还没有调用点。
    #[allow(dead_code)]
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
    /// 复制一个句柄：弱计数加 1。
    fn clone(&self) -> Self {
        // SAFETY: Retain 自己保证 WeakChunk 活着
        let weak = unsafe { self.weak_chunk_.as_ref() };
        weak.incr_weak_count();
        Retain {
            weak_chunk_: self.weak_chunk_,
        }
    }
}

impl<T> Drop for Retain<T>
where
    T: ?Sized,
{
    /// 释放一个句柄：弱计数减 1；减到 0 时尝试走"确定性析构"。
    fn drop(&mut self) {
        // SAFETY: Retain 自己保证 WeakChunk 活着；T 只影响类型，不影响槽位布局
        let weak = unsafe { self.weak_chunk_.cast::<WeakChunk<()>>().as_ref() };
        if weak.decr_weak_count() != 0 {
            return;
        }
        // 弱计数归零：若此刻没有强引用（状态为 Created），立即走来源 (a)。
        // 若状态仍是 Owning / Sharing（只可能来自泄漏或 unsafe），留给清盘。
        let _ = claim_and_destroy_if_unreachable_(weak);
    }
}

/// 走"确定性析构"来源 (a)：当且仅当"弱计数为 0 且状态为 `Created`"时认领并执行清理。
///
/// 这是三处引用计数收尾（`Owning::drop` / `Sharing::drop` / `Retain::drop`）共用的判断，
/// 保证"恰好销毁一次"只由 `try_claim_destroy` 的 CAS 裁决。返回本次是否完成了析构。
fn claim_and_destroy_if_unreachable_(weak: &WeakChunk<()>) -> bool {
    if weak.weak_count() != 0 || weak.data_state() != DataState::Created {
        return false;
    }
    if weak.chunk_state_.try_claim_destroy().is_none() {
        return false;
    }
    // SAFETY: 刚由本调用认领成功，且本路径恰好执行一次
    unsafe { weak.drop_data() };
    weak.chunk_state_.mark_destroyed();
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
