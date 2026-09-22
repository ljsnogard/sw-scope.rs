//! 字宽原子量抽象与内存序配置：`atomex` 相关子集的本地实现（不引 `atomex`）。
//!
//! - [`TrAtomicCell`]：把 `AtomicU32` / `AtomicUsize` 统一成"字宽原子量"；
//! - [`TrAtomicData`]：值类型到其原子单元的映射，并给出字宽与零元 / 一；
//! - [`TrBitWord`]：可参与位运算的字宽值，供"取某一位当锁信号"使用；
//! - [`TrCmpxchOrderings`]：CAS 成功 / 失败与读取使用的内存序，可配置。

use core::{
    ops::{BitAnd, BitOr, Not, Shl},
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

/// 字宽原子量：把 `AtomicU32` / `AtomicUsize` 等统一成同一套读写接口。
pub(crate) trait TrAtomicCell: Sized {
    /// 底层原生值类型。
    type Value: Copy;

    /// 构造一个原子单元。
    fn new(value: Self::Value) -> Self;

    /// 读取当前值。
    fn load(&self, order: Ordering) -> Self::Value;

    /// 条件写入；`weak` 版本允许伪失败。
    fn compare_exchange_weak(
        &self,
        current: Self::Value,
        desired: Self::Value,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Self::Value, Self::Value>;

    /// 原子加，返回加之前的值。
    fn fetch_add(&self, value: Self::Value, order: Ordering) -> Self::Value;

    /// 原子减，返回减之前的值。
    fn fetch_sub(&self, value: Self::Value, order: Ordering) -> Self::Value;
}

macro_rules! impl_tr_atomic_cell {
    ($cell:ident : $value:ty) => {
        impl TrAtomicCell for $cell {
            type Value = $value;

            #[inline]
            fn new(value: $value) -> Self {
                <$cell>::new(value)
            }

            #[inline]
            fn load(&self, order: Ordering) -> $value {
                <$cell>::load(self, order)
            }

            #[inline]
            fn compare_exchange_weak(
                &self,
                current: $value,
                desired: $value,
                success: Ordering,
                failure: Ordering,
            ) -> Result<$value, $value> {
                <$cell>::compare_exchange_weak(self, current, desired, success, failure)
            }

            #[inline]
            fn fetch_add(&self, value: $value, order: Ordering) -> $value {
                <$cell>::fetch_add(self, value, order)
            }

            #[inline]
            fn fetch_sub(&self, value: $value, order: Ordering) -> $value {
                <$cell>::fetch_sub(self, value, order)
            }
        }
    };
}

impl_tr_atomic_cell!(AtomicU32 : u32);
impl_tr_atomic_cell!(AtomicUsize : usize);

/// 值类型到其原子单元的映射，并给出字宽与零元 / 一。
pub(crate) trait TrAtomicData: Copy {
    /// 承载该值的原子单元。
    type AtomicCell: TrAtomicCell<Value = Self>;

    /// 字宽（以位计）。
    const BITS: u32;
    /// 全零位模式。
    const ZERO: Self;
    /// 仅最低位为 1 的位模式。
    const ONE: Self;
}

impl TrAtomicData for u32 {
    type AtomicCell = AtomicU32;

    const BITS: u32 = u32::BITS;
    const ZERO: u32 = 0;
    const ONE: u32 = 1;
}

impl TrAtomicData for usize {
    type AtomicCell = AtomicUsize;

    const BITS: u32 = usize::BITS;
    const ZERO: usize = 0;
    const ONE: usize = 1;
}

/// 可参与位运算的字宽值。
///
/// "把字里的哪一位当锁信号"要求这些运算，因此锁信号策略以本 trait 为界；不实现本 trait 的
/// 值类型（如 `bool`）不能用于位信号锁。
pub(crate) trait TrBitWord:
    TrAtomicData
    + BitAnd<Output = Self>
    + BitOr<Output = Self>
    + Not<Output = Self>
    + Shl<u32, Output = Self>
    + PartialEq
{}

impl<T> TrBitWord for T where
    T: TrAtomicData
        + BitAnd<Output = T>
        + BitOr<Output = T>
        + Not<Output = T>
        + Shl<u32, Output = T>
        + PartialEq
{}

/// CAS 与读取使用的内存序，作为锁的 `O` 配置项。
pub(crate) trait TrCmpxchOrderings {
    /// CAS 成功时使用的序。
    const SUCC_ORDERING: Ordering;
    /// CAS 失败时使用的序。
    const FAIL_ORDERING: Ordering;
    /// 普通读取使用的序。
    const LOAD_ORDERING: Ordering;
}

/// 最严格的内存序，代价是更高的开销；不确定用哪个时选它。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StrictOrderings;

impl TrCmpxchOrderings for StrictOrderings {
    const SUCC_ORDERING: Ordering = Ordering::SeqCst;
    const FAIL_ORDERING: Ordering = Ordering::SeqCst;
    const LOAD_ORDERING: Ordering = Ordering::SeqCst;
}

/// 面向锁的宽松内存序。
///
/// 当前只有测试使用它来验证 `O` 可配置；生产路径默认走 [`StrictOrderings`]。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LocksOrderings;

impl TrCmpxchOrderings for LocksOrderings {
    const SUCC_ORDERING: Ordering = Ordering::Acquire;
    const FAIL_ORDERING: Ordering = Ordering::Relaxed;
    const LOAD_ORDERING: Ordering = Ordering::Acquire;
}
