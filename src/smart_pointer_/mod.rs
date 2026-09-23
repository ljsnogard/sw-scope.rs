//! 三种智能指针：`Retain` / `Owning` / `Sharing`。
//!
//! 对象可以直接经 `Scope` 放进 arena 并以 `Owning` / `Sharing` 访问（不必先有 `Retain`），
//! 也可以先用 `Owning::retained()` / `Sharing::retained()` 取得 `Retain`，再从 `Retain`
//! 升级。`Retain` 是**可选**的身份句柄：一旦取得，它会为对象补建 `WeakChunk` 身份槽位，
//! 并在强计数归零后继续把数据吊住，直到最后一个 `Retain` 消失或 Scope 清盘。
//!
//! 当前阶段三者都保持 `!Send + !Sync`。

mod owning_;
mod retain_;
mod sharing_;

#[cfg(test)]
mod tests_;

pub use owning_::Owning;
pub use retain_::Retain;
pub use sharing_::Sharing;
