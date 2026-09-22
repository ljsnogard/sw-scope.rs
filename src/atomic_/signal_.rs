//! 锁信号策略：决定"原子字里的哪一位代表已上锁"。
//!
//! 这是 [`SpinMutex`](super::SpinMutex) 与 [`SpinFlag`](super::SpinFlag) 的 `S` 配置项。
//! 默认 [`MsbAsMutexSignal`] 取最高位，[`LsbAsMutexSignal`] 取最低位；需要别的位型时，为自定义
//! 类型实现 [`TrMutexSignal`] 并把这个类型放到 `S` 位置即可，锁本身的实现无需改动。

use core::marker::PhantomData;

use super::cell_::TrBitWord;

/// 自旋锁信号的行为：如何判定已上锁 / 已释放，以及如何在两者间切换。
pub(crate) trait TrMutexSignal<V>
where
    V: Copy,
{
    /// 该值是否表示"已上锁"。
    fn is_acquired(val: V) -> bool;

    /// 该值是否表示"已释放"。
    fn is_released(val: V) -> bool {
        !Self::is_acquired(val)
    }

    /// 置为"已上锁"。
    fn make_acquired(val: V) -> V;

    /// 置为"已释放"。
    fn make_released(val: V) -> V;
}

/// 以最高位为锁信号（默认策略）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct MsbAsMutexSignal<V: TrBitWord>(PhantomData<V>);

impl<V: TrBitWord> MsbAsMutexSignal<V> {
    /// 锁信号位。
    #[inline(always)]
    fn flag_() -> V {
        V::ONE << (V::BITS - 1)
    }
}

impl<V: TrBitWord> TrMutexSignal<V> for MsbAsMutexSignal<V> {
    fn is_acquired(val: V) -> bool {
        (val & Self::flag_()) == Self::flag_()
    }

    fn make_acquired(val: V) -> V {
        val | Self::flag_()
    }

    fn make_released(val: V) -> V {
        val & !Self::flag_()
    }
}

/// 以最低位为锁信号。
///
/// 与 [`MsbAsMutexSignal`] 只是"取位位置"不同，用来证明锁信号位确实是可配置项。
/// 当前只有测试使用它，生产路径统一走最高位。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct LsbAsMutexSignal<V: TrBitWord>(PhantomData<V>);

#[allow(dead_code)] // 同上的测试用途
impl<V: TrBitWord> LsbAsMutexSignal<V> {
    /// 锁信号位。
    #[inline(always)]
    fn flag_() -> V {
        V::ONE
    }
}

impl<V: TrBitWord> TrMutexSignal<V> for LsbAsMutexSignal<V> {
    fn is_acquired(val: V) -> bool {
        (val & Self::flag_()) == Self::flag_()
    }

    fn make_acquired(val: V) -> V {
        val | Self::flag_()
    }

    fn make_released(val: V) -> V {
        val & !Self::flag_()
    }
}
