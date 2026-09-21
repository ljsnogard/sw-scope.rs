use core::{
    alloc::Layout,
    mem,
    ptr::{self, NonNull},
};

use crate::scope_inner_::PoolIndex;

/// 承载 [`crate::strong_::StrongChunk`] 的内嵌式内存池。
///
/// 池头自身位于块首，cell 数组紧随其后，因此由任意 cell 地址减去池头大小即可反推池地址。
/// [`PoolIndex`] 只有 16 位，多数情况下 `CELL_SIZE` 是 8，于是一个 `StrongPool` 的大小
/// 通常不会超过 `2^(16 + 3) = 512 KB`。
///
/// # 位置终身不变（重要前提）
///
/// 本池采用纯 bump 分配：cell 一律从前往后顺序发放，`used_count_` 只增不减，而且**不给
/// 中间的空洞做复用**（这是 Scope 的既定策略：宁可整块清盘，也不花时间去搜罗零散可复用
/// 空间）。由此得到一条被上层依赖的不变量：
///
/// > 一旦给某个对象分配了位置，该位置在对象的整个生命周期内都不会移动，也不会被别的
/// > 对象占用。
///
/// 对象身份（`WeakChunk`）里保存的强块指针、以及存活链上的链接，都建立在这条不变量上：
/// 只要对象还活着，它的地址就是稳定的。整块池子只在 Scope 静默时一次性回收。
///
/// # 是否需要开新池
///
/// `used_count_` 与 `cell_count_` 的差额就是"本池还能再放多少 cell"，上层据此决定是继续
/// 从本池分配，还是再申请一个 `StrongPool` 串到链上。这就是 `used_count_` 必须保留的原因。
#[repr(C)]
pub(crate) struct StrongPool<const CELL_SIZE: usize> {
    /// 上一个内存链块地址
    prev_pool_: Option<NonNull<StrongPool<CELL_SIZE>>>,
    /// 荷载可用的 cell 的数量
    cell_count_: PoolIndex,
    /// 分配已用的 cell 的数量
    used_count_: PoolIndex,
}

impl<const CELL_SIZE: usize> StrongPool<CELL_SIZE> {
    /// 池头自身的大小。
    pub(crate) const THIS_SIZE: usize = mem::size_of::<Self>();

    /// 整块池子承载的全部 cell 空间。
    pub(crate) const fn memory_(&self) -> NonNull<[u8]> {
        let p = self as *const Self as *const u8;
        unsafe {
            let data = p.add(Self::THIS_SIZE);
            let len = (self.cell_count_ as usize) * CELL_SIZE;
            let slice = ptr::slice_from_raw_parts(data, len);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    /// 尚未分配出去的那段 cell 空间，从第一个空闲位置直到池尾。
    pub(crate) const fn free_addr_(&self) -> NonNull<[u8]> {
        let p = self as *const Self as *const u8;
        let offset = (self.used_count_ as usize) * CELL_SIZE;
        unsafe {
            let data = p.add(Self::THIS_SIZE + offset);
            let len = (self.cell_count_ as usize) * CELL_SIZE;
            let slice = ptr::slice_from_raw_parts(data, len);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    /// 尝试分配，若容量不足，返回最大能分配的长度。
    pub(crate) fn allocate_(
        &mut self,
        layout: Layout,
    ) -> Result<NonNull<[u8]>, usize> {
        todo!("[StrongPool::allocate_] not implemented")
    }
}
