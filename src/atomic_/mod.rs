//! 原子量原语与自旋锁。
//!
//! - `cell_`：`AtomicU32` / `AtomicUsize` 的字宽抽象、位宽值、内存序配置；
//! - `signal_`：**锁信号位**的策略（哪一位表示已上锁）；
//! - `spin_`：共享锁操作与两种锁——[`SpinMutex`]（锁住任意内容）、[`SpinFlag`]（锁住一个
//!   带业务位的原子字）。
//!
//! 对外只导出使用方需要按名字引用的项（`ScopeInner::flags_` 的接入见
//! `dev-notes/scope-20260922-1423.md` §8，本轮只提供锁本身）。

mod cell_;
mod signal_;
mod spin_;

#[cfg(test)]
mod tests_;

pub(crate) use signal_::MsbAsMutexSignal;
pub(crate) use spin_::SpinFlag;
pub(crate) use spin_::SpinMutex;
