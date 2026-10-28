//! 单元测试共用的测试夹具。
//!
//! 生产路径不再有隐式默认 root，因此每个需要 root 的测试都显式创建自己的 root。这里
//! 用 `Box::leak` 提供一个 `'static` 的发布槽位；槽位内存随测试进程一起结束，而 root
//! 本身是否被最终清理由测试自行决定（调用 `RootScope::close` 的测试会真正释放它）。

use alloc::boxed::Box;
use core::{ptr, sync::atomic::AtomicPtr};
use std::alloc::Global;

use crate::{RootScope, scope_inner_::DEFAULT_PAGE_SIZE};

/// 为单元测试创建一个独立的 root。
///
/// 每次调用都会泄漏一个 `AtomicPtr` 槽位（测试进程内可忽略），但 root 是全新的，
/// 不会与其它测试共享类型表或实体池。
pub(crate) fn test_root_() -> &'static RootScope {
    let slot: &'static mut AtomicPtr<RootScope> =
        Box::leak(Box::new(AtomicPtr::new(ptr::null_mut())));
    match RootScope::try_init_in(slot, DEFAULT_PAGE_SIZE, Global) {
        Result::Ok(root) | Result::Err(root) => root,
    }
}
