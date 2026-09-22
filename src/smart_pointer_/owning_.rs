//! `Owning<'a, T, M = Local>`：类似 `Box<T>` 的 scope 独占访问句柄。
//!
//! 当前只为 `M = Local` 实现；它只归还访问权，不负责析构数据。

use core::{marker::PhantomData, ptr::NonNull};

use crate::{
    abs_::TrShareMarker,
    share_marker_::Local,
    strong_::StrongChunk,
    weak_::DataState,
};

use super::retain_::claim_and_destroy_if_unreachable_;

/// Like Box<T> but scoped.
pub struct Owning<'a, T, M = Local>
where
    T: 'a + ?Sized,
    M: TrShareMarker,
{
    strong_chunk_: NonNull<StrongChunk<T>>,
    _lifetime_a_: PhantomData<&'a T>,
    _mode_: PhantomData<M>,
}

impl<'a, T> Owning<'a, T, Local>
where
    T: 'a + ?Sized,
{
    pub(crate) const fn new(chunk: NonNull<StrongChunk<T>>) -> Self {
        Owning {
            strong_chunk_: chunk,
            _lifetime_a_: PhantomData,
            _mode_: PhantomData,
        }
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
        // 状态归还与强计数/后续升级共用槽位锁；锁在语句块结束时释放，然后再尝试
        // 确定性析构。正常路径下弱计数 > 0，claim 不会触发；保留判断以覆盖泄漏 / unsafe。
        {
            let guard = weak.lock_busy();
            let _ = guard
                .chunk_state()
                .try_transition_state(DataState::Owning, DataState::Created);
        }
        let _ = claim_and_destroy_if_unreachable_(weak);
    }
}
