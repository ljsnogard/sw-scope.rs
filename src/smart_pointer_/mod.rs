//! 三种智能指针：`Retain` / `Owning` / `Sharing`。
//!
//! `Retain` 是对象身份与升级入口；`Owning` / `Sharing` 是由 `Retain` 借出的临时访问句柄。
//! 当前阶段三者都保持 `!Send + !Sync`。

mod owning_;
mod retain_;
mod sharing_;

#[cfg(test)]
mod tests_;

pub use owning_::Owning;
pub use retain_::Retain;
pub use sharing_::Sharing;
