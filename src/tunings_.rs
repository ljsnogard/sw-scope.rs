/// 旧的分块大小调优入口。
///
/// 新实现把“池块大小”交给 `StrongPool` / `WeakPool` 的构造参数与 root 页大小决定，
/// 不再从类型级常量读取；本 trait 因此没有实现者，保留是为了不打断旧设计文档里的
/// 引用。
#[allow(
    dead_code,
    reason = "旧调优入口：新池按构造参数与 root 页大小决定块尺寸，trait 暂无实现者"
)]
pub trait TrScopeTunings {
    /// 保存 StrongChunk 的内存块大小，最多不要超过 32K 个指针的大小
    const STRONG_CHUNK_BLOCK_SIZE: usize;

    /// 保存 WeakSlot 的内存块大小，最多不要超过 32K 个 WeakSlot 的大小
    const WEAK_SLOT_BLOCK_SIZE: usize;
}
