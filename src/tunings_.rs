pub trait TrScopeTunings {
    /// 保存 StrongChunk 的内存块大小，最多不要超过 32K 个指针的大小
    const STRONG_CHUNK_BLOCK_SIZE: usize;

    /// 保存 WeakSlot 的内存块大小，最多不要超过 32K 个 WeakSlot 的大小
    const WEAK_SLOT_BLOCK_SIZE: usize;
}
