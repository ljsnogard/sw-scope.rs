//! 自旋锁：`SpinMutex` 锁住任意内容，`SpinFlag` 锁住一个"锁位 + 业务位"的原子字。
//!
//! 两个类型共用同一套锁操作（本文件顶部的 [`try_acquire`] / [`release`] / [`try_update`]）：
//! **`compare_exchange` 只出现在这里**，业务代码只调用"抢锁 / 释放 / 条件读改写"这些语义操作。
//!
//! 泛型参数含义：
//!
//! | 参数 | 含义 |
//! | --- | --- |
//! | `D` / `C` | 底层原子字的值类型与原子单元类型（如 `u32` / `AtomicU32`） |
//! | `S` | **锁信号策略**：字里的哪一位表示已上锁（默认最高位） |
//! | `O` | CAS 使用的内存序（默认 [`StrictOrderings`]） |
//! | `T` | 仅 [`SpinMutex`]：**被锁住的内容**，只有持锁者能拿到 `&mut T` |
//!
//! 沿用 `strong-20260921-2210.md` §8 的约定：**不存在对外的手动解锁入口**，解锁只发生在守卫
//! 的 `Drop` 里；抢锁是唯一可能 panic 的步骤，且发生在持有锁之前。

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use super::cell_::{
    StrictOrderings, TrAtomicCell, TrAtomicData, TrCmpxchOrderings, TrBitWord,
};
use super::signal_::{MsbAsMutexSignal, TrMutexSignal};

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 共享锁操作：CAS 的唯一所在

/// 按信号策略抢锁；`max_try == 0` 表示忙等（必然成功）。
pub(crate) fn try_acquire<D, C, S, O>(cell: &C, max_try: usize) -> bool
where
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    let mut current = cell.load(O::LOAD_ORDERING);
    let mut tried = 0usize;
    loop {
        if S::is_released(current) {
            let acquired = S::make_acquired(current);
            match cell.compare_exchange_weak(
                current,
                acquired,
                O::SUCC_ORDERING,
                O::FAIL_ORDERING,
            ) {
                Result::Ok(_) => return true,
                Result::Err(observed) => current = observed,
            }
        } else {
            // 判据不成立说明锁被占着；重新读取，避免拿着过期值空转
            core::hint::spin_loop();
            current = cell.load(O::LOAD_ORDERING);
        }
        tried += 1;
        if max_try != 0 && tried >= max_try {
            return false;
        }
    }
}

/// 释放锁；返回是否确实由本次调用完成释放。
fn release_<D, C, S, O>(cell: &C) -> bool
where
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    let mut current = cell.load(O::LOAD_ORDERING);
    loop {
        if S::is_released(current) {
            return false;
        }
        let released = S::make_released(current);
        match cell.compare_exchange_weak(current, released, O::SUCC_ORDERING, O::FAIL_ORDERING) {
            Result::Ok(_) => return true,
            Result::Err(observed) => current = observed,
        }
    }
}

/// 无锁的条件读—改—写：`op` 收到**剔除锁位后**的字，返回 `(新字, 结果)`。
///
/// - `op` 返回 [`None`] 表示"前提不成立"，放弃并返回 [`None`]；
/// - 成功写回时保持锁位原状，因此本操作不会与持锁者互相破坏；
/// - `op` 可能因并发踩踏被调用多次，必须是幂等的纯计算。
pub(crate) fn try_update<D, C, S, O, R>(cell: &C, mut op: impl FnMut(D) -> Option<(D, R)>) -> Option<R>
where
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    let mut current = cell.load(O::LOAD_ORDERING);
    loop {
        let visible = S::make_released(current);
        let (next_visible, out) = op(visible)?;
        let next = if S::is_acquired(current) {
            S::make_acquired(S::make_released(next_visible))
        } else {
            S::make_released(next_visible)
        };
        if next == current {
            return Option::Some(out);
        }
        match cell.compare_exchange_weak(current, next, O::SUCC_ORDERING, O::FAIL_ORDERING) {
            Result::Ok(_) => return Option::Some(out),
            Result::Err(observed) => current = observed,
        }
    }
}

/// 读当前字（锁位由调用方按需解释）。
fn load_<D, C, O>(cell: &C) -> D
where
    D: TrAtomicData,
    C: TrAtomicCell<Value = D>,
    O: TrCmpxchOrderings,
{
    cell.load(O::LOAD_ORDERING)
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- SpinMutex：锁住任意内容

/// 互斥自旋锁：被锁内容 `T` 与锁信号策略 `S` 都是泛型参数。
pub(crate) struct SpinMutex<
    T,
    D = usize,
    C = AtomicUsize,
    S = MsbAsMutexSignal<D>,
    O = StrictOrderings,
> where
    T: ?Sized,
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// 状态字；信号位由 `S` 解释。
    state_: C,
    /// 类型占位。
    _use_d_: PhantomData<D>,
    /// 锁信号策略占位。
    _use_s_: PhantomData<S>,
    /// 内存序占位。
    _use_o_: PhantomData<O>,
    /// 受保护的数据。`T` 可能 unsized，因此必须是最后一个字段。
    value_: UnsafeCell<T>,
}

impl<T, D, C, S, O> SpinMutex<T, D, C, S, O>
where
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// 建一把未上锁的锁，并放入初值。
    pub(crate) fn new(value: T) -> Self {
        SpinMutex {
            state_: C::new(D::ZERO),
            _use_d_: PhantomData,
            _use_s_: PhantomData,
            _use_o_: PhantomData,
            value_: UnsafeCell::new(value),
        }
    }
}

impl<T, D, C, S, O> SpinMutex<T, D, C, S, O>
where
    T: ?Sized,
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// 忙等抢锁，然后在临界区内执行闭包。
    ///
    /// 闭包 panic 时 [`SpinMutexGuard`] 仍会析构并释放锁。
    pub(crate) fn with_locked<R>(&self, access: impl FnOnce(&mut T) -> R) -> R {
        let _guard = self.lock();
        // SAFETY: 持锁即独占，且守卫在本函数返回前不会析构
        access(unsafe { &mut *self.value_.get() })
    }

    /// 有界重试抢锁；抢不到返回 [`None`]。`max_try == 0` 表示忙等（必然成功）。
    ///
    /// 预留给"抢不到就返回"的调用点；当前仅测试使用。
    #[allow(dead_code)]
    pub(crate) fn try_with_locked<R>(
        &self,
        max_try: usize,
        access: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let _guard = self.try_lock(max_try)?;
        // SAFETY: 持锁即独占，且守卫在本函数返回前不会析构
        Option::Some(access(unsafe { &mut *self.value_.get() }))
    }

    /// 当前是否已上锁。
    #[allow(dead_code)] // 预留给诊断与断言；当前仅测试使用
    pub(crate) fn is_acquired(&self) -> bool {
        S::is_acquired(load_::<D, C, O>(&self.state_))
    }

    /// 忙等抢锁并交出守卫。
    fn lock(&self) -> SpinMutexGuard<'_, T, D, C, S, O> {
        match self.try_lock(0) {
            Option::Some(guard) => guard,
            // 不限次数的抢锁只在"锁永远不释放"时才会走到这里，属于逻辑错误
            Option::None => unreachable!("不限次数的抢锁必定成功"),
        }
    }

    /// 有界重试抢锁并交出守卫。
    fn try_lock(&self, max_try: usize) -> Option<SpinMutexGuard<'_, T, D, C, S, O>> {
        if try_acquire::<D, C, S, O>(&self.state_, max_try) {
            Option::Some(SpinMutexGuard { mutex_: self })
        } else {
            Option::None
        }
    }

    /// 释放锁。**只应由 [`SpinMutexGuard`] 的 `Drop` 调用。**
    fn release(&self) -> bool {
        release_::<D, C, S, O>(&self.state_)
    }
}

// SAFETY: 同一时刻只有一个线程能通过 [`SpinMutexGuard`] 拿到 `&mut T`，其效果等价于把 `T` 在
// 线程之间转移，因此跨线程能力的判据与 `T: Send` 一致；状态字与三个标记类型本身可跨线程。
unsafe impl<T, D, C, S, O> Send for SpinMutex<T, D, C, S, O>
where
    T: ?Sized + Send,
    D: TrBitWord + Send,
    C: TrAtomicCell<Value = D> + Send + Sync,
    S: TrMutexSignal<D> + Send + Sync,
    O: TrCmpxchOrderings + Send + Sync,
{
}

// SAFETY: 见上；`&SpinMutex` 只允许通过 CAS 取得独占访问权，不会产生数据竞争。
unsafe impl<T, D, C, S, O> Sync for SpinMutex<T, D, C, S, O>
where
    T: ?Sized + Send,
    D: TrBitWord + Send,
    C: TrAtomicCell<Value = D> + Send + Sync,
    S: TrMutexSignal<D> + Send + Sync,
    O: TrCmpxchOrderings + Send + Sync,
{
}

/// 持有 [`SpinMutex`] 的守卫：析构即解锁，`Deref` / `DerefMut` 给出锁内数据。
pub(crate) struct SpinMutexGuard<'a, T, D, C, S, O>
where
    T: ?Sized + 'a,
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// 守卫对应的锁。
    mutex_: &'a SpinMutex<T, D, C, S, O>,
}

impl<T, D, C, S, O> Deref for SpinMutexGuard<'_, T, D, C, S, O>
where
    T: ?Sized,
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: 守卫存在即代表持锁，因而独占
        unsafe { &*self.mutex_.value_.get() }
    }
}

impl<T, D, C, S, O> DerefMut for SpinMutexGuard<'_, T, D, C, S, O>
where
    T: ?Sized,
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: 守卫存在即代表持锁，因而独占
        unsafe { &mut *self.mutex_.value_.get() }
    }
}

impl<T, D, C, S, O> Drop for SpinMutexGuard<'_, T, D, C, S, O>
where
    T: ?Sized,
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// **解锁的唯一发生点。**
    fn drop(&mut self) {
        let _ = self.mutex_.release();
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- SpinFlag：锁住一个"锁位 + 业务位"的原子字

/// 字锁：锁位由 `S` 给出，其余位是调用方自己的业务数据。
///
/// 与 [`SpinMutex`] 的区别是"被锁内容"就是原子字本身——状态与锁位必须共用同一个字时用它
/// （例如 `WeakChunkState` 的状态字）。所有 CAS / 掩码运算都封装在本类型的操作里。
#[repr(transparent)]
pub(crate) struct SpinFlag<
    D = u32,
    C = AtomicU32,
    S = MsbAsMutexSignal<D>,
    O = StrictOrderings,
> where
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// 状态字；锁位由 `S` 解释。
    bits_: C,
    /// 类型占位。
    _use_d_: PhantomData<D>,
    /// 锁信号策略占位。
    _use_s_: PhantomData<S>,
    /// 内存序占位。
    _use_o_: PhantomData<O>,
}

impl<D, C, S, O> SpinFlag<D, C, S, O>
where
    D: TrBitWord,
    C: TrAtomicCell<Value = D>,
    S: TrMutexSignal<D>,
    O: TrCmpxchOrderings,
{
    /// 建一个字锁；`bits` 里的锁位会被剔除。
    pub(crate) fn new(bits: D) -> Self {
        SpinFlag {
            bits_: C::new(S::make_released(bits)),
            _use_d_: PhantomData,
            _use_s_: PhantomData,
            _use_o_: PhantomData,
        }
    }

    /// 读业务位（**不含**锁位），无锁快路径。
    #[inline]
    pub fn read(&self) -> D {
        S::make_released(load_::<D, C, O>(&self.bits_))
    }

    /// 当前是否已上锁。
    #[inline]
    pub fn is_locked(&self) -> bool {
        S::is_acquired(load_::<D, C, O>(&self.bits_))
    }

    /// 按信号策略抢锁；`max_try == 0` 表示忙等。
    ///
    /// 抢锁入口：只应由本类型的守卫（`WeakChunkStateGuard`）使用，并由它在 `Drop` 里
    /// 调用 [`release`](SpinFlag::release)。
    pub fn try_acquire(&self, max_try: usize) -> bool {
        try_acquire::<D, C, S, O>(&self.bits_, max_try)
    }

    /// 释放锁；返回是否确实由本次调用完成释放。
    ///
    /// **只应由持锁守卫的 `Drop` 调用。**
    pub fn release(&self) -> bool {
        release_::<D, C, S, O>(&self.bits_)
    }

    /// 无锁的条件读—改—写：`op` 收到业务位，返回 `(新业务位, 结果)`。
    ///
    /// `op` 返回 [`None`] 表示前提不成立、放弃。锁位在写回时保持原状。
    pub fn try_update<R>(&self, op: impl FnMut(D) -> Option<(D, R)>) -> Option<R> {
        try_update::<D, C, S, O, R>(&self.bits_, op)
    }

    /// 业务位算术加（用于弱引用计数这类低位计数）。
    #[inline]
    pub(crate) fn fetch_add(&self, value: D) -> D {
        self.bits_.fetch_add(value, Ordering::AcqRel)
    }

    /// 业务位算术减。
    #[inline]
    pub(crate) fn fetch_sub(&self, value: D) -> D {
        self.bits_.fetch_sub(value, Ordering::AcqRel)
    }
}
