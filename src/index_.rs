use core::{marker::PhantomData, ptr::NonNull};

use crate::{
    scope_inner_::{RootScope, try_flush_for_slot_},
    strong_::{StrongChunk, StrongChunkBase},
    weak_::{DataState, WeakSlot},
};

/// Like `Box<T>` but scoped.
pub struct Owning<'a, T>
where
    T: 'a + ?Sized,
{
    strong_chunk_: NonNull<StrongChunkBase>,
    _lifetime_a_: PhantomData<&'a T>,
}

/// Like `Arc<T>` but scoped.
pub struct Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    strong_chunk_: NonNull<StrongChunkBase>,
    _lifetime_a_: PhantomData<&'a T>,
}

/// Like a gc handle. The lifetime is the same as the scope that allocates it.
pub struct Retain<T>
where
    T: ?Sized,
{
    /// We are guaranteed that `WeakChunk<T>` always outlives everything in
    /// the `Weak<T>`;
    weak_chunk_: NonNull<WeakSlot<T>>,
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<'a, T> Owning<'a, T>
where
    T: 'a + ?Sized,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunkBase>) -> Self {
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
        // SAFETY: 强块指针来自同一个 T 的分配；头部视图地址与块首址相同
        let chunk = unsafe { &*(self.strong_chunk_.as_ptr() as *const StrongChunk<T>) };
        debug_assert!(chunk.is_data_alive());
        chunk
            .try_get_data()
            .expect("[Owning::deref] should always work.")
    }
}

impl<'a, T> core::ops::DerefMut for Owning<'a, T>
where
    T: 'a + ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: 同上；&mut self 保证独占
        let chunk = unsafe { &mut *(self.strong_chunk_.as_ptr() as *mut StrongChunk<T>) };
        debug_assert!(chunk.is_data_alive());
        chunk
            .try_get_data_mut()
            .expect("[Owning::deref_mut] should always work.")
    }
}

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
        // SAFETY: 强块指针有效；头部视图地址与块首址相同
        let chunk = unsafe { &*(self.strong_chunk_.as_ptr() as *const StrongChunk<T>) };
        // SAFETY: 同上
        let weak = unsafe { chunk.weak_chunk().as_ref() };
        debug_assert!(
            weak.weak_count() > 0,
            "Owning 必然借用自某个 Retain，弱计数不该是 0"
        );
        let _ = weak
            .chunk_state()
            .try_transition_state(DataState::Owning, DataState::Created);
        // 正常路径下弱计数 > 0，这里不会触发；保留判断以覆盖泄漏 / unsafe 组合
        let _ = claim_and_destroy_if_unreachable_(weak);
        // 若本域已 close，且刚刚释放的是最后一个直接视图，重试延迟清盘。
        try_flush_for_slot_(weak);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<'a, T> Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunkBase>) -> Self {
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
        // SAFETY: 强块指针来自同一个 T 的分配；头部视图地址与块首址相同
        let chunk = unsafe { &*(self.strong_chunk_.as_ptr() as *const StrongChunk<T>) };
        debug_assert!(chunk.is_data_alive());
        chunk
            .try_get_data()
            .expect("[Owning::deref] should always work.")
    }
}

impl<'a, T> Clone for Sharing<'a, T>
where
    T: 'a + ?Sized,
{
    /// 复制一个共享句柄：只把强计数加 1。
    fn clone(&self) -> Self {
        // SAFETY: Sharing 持有强块指针，强块活着时它有效
        // SAFETY: 头部指针有效
        let chunk = unsafe { &*(self.strong_chunk_.as_ptr() as *const StrongChunk<T>) };
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
        // SAFETY: 头部指针有效
        let chunk = unsafe { &*(self.strong_chunk_.as_ptr() as *const StrongChunk<T>) };
        // SAFETY: 同上
        let weak = unsafe { chunk.weak_chunk().as_ref() };
        if chunk.decr_strong_count() == 0 {
            let _ = weak
                .chunk_state()
                .try_transition_state(DataState::Sharing, DataState::Created);
            let _ = claim_and_destroy_if_unreachable_(weak);
        }
        // 若本域已 close，且最后一个 Sharing 已释放，重试延迟清盘。
        try_flush_for_slot_(weak);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

impl<T> Retain<T>
where
    T: ?Sized,
{
    /// 尝试从 `Weak<T>` 提升为 `Retain<T>`。
    /// 当且仅当 `DataState::Allocated` 时会成功
    pub fn try_owning(&self) -> Option<Owning<'_, T>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        let strong = chunk.try_owning().ok()?;
        Option::Some(Owning::new(raw_to_strong_::<T>(strong)))
    }

    /// 尝试从 `Weak<T>` 提升为 `Shared<T>`。
    /// 当且仅当 `DataState::Allocated` 时会成功。
    pub fn try_sharing(&self) -> Option<Sharing<'_, T>> {
        let chunk = unsafe { self.weak_chunk_.as_ref() };
        let strong = chunk.try_sharing().ok()?;
        Option::Some(Sharing::new(raw_to_strong_::<T>(strong)))
    }
    /// 构造一个句柄。分配管线（Batch 2）落地前，生产路径还没有调用点。
    #[allow(dead_code)]
    pub(crate) const fn new(chunk: NonNull<WeakSlot<T>>) -> Self {
        Retain { weak_chunk_: chunk }
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
        let weak = unsafe { self.weak_chunk_.cast::<WeakSlot<()>>().as_ref() };
        if weak.decr_weak_count() != 0 {
            return;
        }
        // 弱计数归零：若此刻没有强引用（状态为 Created），立即走来源 (a)。
        // 若状态仍是 Owning / Sharing（只可能来自泄漏或 unsafe），留给清盘。
        let _ = claim_and_destroy_if_unreachable_(weak);
        // 若本域已 close，最后一个 Retain 消失后重试清盘；这样并发/级联析构的
        // 收尾也能把强池还掉。
        try_flush_for_slot_(weak);
    }
}

/// 走"确定性析构"来源 (a)：当且仅当"弱计数为 0 且状态为 `Created`"时认领并执行清理。
///
/// 这是三处引用计数收尾（`Owning::drop` / `Sharing::drop` / `Retain::drop`）共用的判断，
/// 保证"恰好销毁一次"只由 `try_claim_destroy` 的 CAS 裁决。返回本次是否完成了析构。
fn claim_and_destroy_if_unreachable_(weak: &WeakSlot<()>) -> bool {
    if weak.weak_count() != 0 || weak.data_state() != DataState::Created {
        return false;
    }
    // SAFETY: 弱计数为 0 且状态为 Created，没有直接数据视图；下面立即认领
    unsafe { force_destroy_slot_(weak) }
}

/// 强制认领并析构一个槽位；成功认领返回 `true`，已被其他路径处理返回 `false`。
///
/// 这是“确定性析构”和“Scope::close 延迟清盘”共用的入口：唯一裁决仍是
/// `try_claim_destroy`，因此并发/级联路径不会重复析构。
///
/// # Safety
///
/// 调用方必须保证没有 `Owning` / `Sharing` 视图，且强块数据尚未析构。
pub(crate) unsafe fn force_destroy_slot_(weak: &WeakSlot<()>) -> bool {
    if weak.chunk_state().try_claim_destroy().is_none() {
        return false;
    }
    // SAFETY: 刚由本调用认领成功，且本路径恰好执行一次
    unsafe { finish_destroy_(weak) };
    true
}

/// 执行已经认领成功的析构：查类型表、跑 Predrop、析构数据并标记 `Destroyed`。
///
/// # Safety
///
/// 调用方必须已经通过 `try_claim_destroy` 成功认领。
unsafe fn finish_destroy_(weak: &WeakSlot<()>) {
    // 清理入口与 `?Sized` 每对象元数据都在强块头部：先经身份槽位找到强块，再用强块
    // 自己的 `type_id_` 去默认 root 的类型表取 vtable。`get_meta_` 返回副本，锁在调用
    // 用户 `Drop` 之前已经释放。
    if let Option::Some(chunk) = weak.strong_chunk() {
        // SAFETY: 刚由调用方认领销毁；强块在弱槽位存活期间一直有效
        let (type_id, data_addr, meta_raw) = {
            let chunk_ref = unsafe { chunk.as_ref() };
            (
                chunk_ref.type_id_(),
                chunk_ref.data_addr_(),
                chunk_ref.pointee_meta_(),
            )
        };
        let vtable = RootScope::default_().and_then(|root| root.get_meta_(type_id));
        match vtable {
            Option::Some(vtable) => {
                // Predrop 对象内联在 chunk 里，定位它需要 chunk 首址；数据本身则只用
                // data_addr + metadata 析构，不依赖 chunk 是否仍然存在。
                // SAFETY: 刚由本调用认领成功、恰好执行一次；chunk 是当初按该 vtable
                // 对应的 `T` / `P` 分配的强块首址
                unsafe { vtable.run_pre_drop_(chunk.as_ptr().cast()) };
                // SAFETY: data_addr / meta_raw 来自同一个强块，数据尚未析构
                unsafe { vtable.run_drop_(data_addr, meta_raw) };
            }
            // 低层测试可能在没有 root 的情况下直接构造 chunk；此时没有类型信息可用，
            // 宁可保守地不调用未知析构，也不做错误的类型投影。生产路径上 root 必然存在，
            // 此时“查不到 vtable”属于不变量被破坏。
            Option::None => debug_assert!(
                RootScope::default_().is_none(),
                "默认 root 已存在，但强块的 type_id 在类型表里查不到 vtable"
            ),
        }
    }
    weak.chunk_state().mark_destroyed();
}

/// 强块头部指针本身就是 typed view 的首址；这里只做类型投影。
fn raw_to_strong_<T: ?Sized>(
    raw: NonNull<crate::strong_::StrongChunkBase>,
) -> NonNull<StrongChunkBase> {
    let _ = core::marker::PhantomData::<T>;
    raw
}
