use core::{
    alloc::{AllocError, Allocator, Layout},
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

    /// 承载 `cell_count` 个 cell 的整块布局（池头 + cell 区）。
    ///
    /// 对齐取"池头对齐"与 `CELL_SIZE` 的较大者，保证 cell 区起点满足 cell 步长。
    pub(crate) fn layout_for_(cell_count: PoolIndex) -> Layout {
        let size = Self::THIS_SIZE + (cell_count as usize) * CELL_SIZE;
        let align = mem::align_of::<Self>().max(CELL_SIZE);
        // SAFETY: size 是池头加上整数个 cell，align 是 2 的幂且不会造成溢出
        unsafe { Layout::from_size_align_unchecked(size, align) }
    }

    /// 申请并初始化一个新池，把它挂在 `prev` 之后。
    ///
    /// # Errors
    ///
    /// `cell_count` 为 0，或底层分配器无法满足布局时返回 [`AllocError`]。
    pub(crate) fn try_new_(
        cell_count: PoolIndex,
        alloc: &'static dyn Allocator,
        prev: Option<NonNull<Self>>,
    ) -> Result<NonNull<Self>, AllocError> {
        if cell_count == 0 {
            return Result::Err(AllocError);
        }
        let mem = alloc.allocate(Self::layout_for_(cell_count))?;
        let base = mem.as_ptr() as *mut u8 as *mut Self;
        // SAFETY: mem 是刚分配、尚无人共享的独占内存，就地写入池头是安全的
        unsafe {
            base.write(StrongPool {
                prev_pool_: prev,
                cell_count_: cell_count,
                used_count_: 0,
            });
            Result::Ok(NonNull::new_unchecked(base))
        }
    }

    /// 本池承载的 cell 总数。
    pub(crate) const fn cell_count_(&self) -> PoolIndex {
        self.cell_count_
    }

    /// 池链上的上一个池。
    pub(crate) const fn prev_(&self) -> Option<NonNull<StrongPool<CELL_SIZE>>> {
        self.prev_pool_
    }

    /// 本池已经用掉的 cell 数。
    ///
    /// 目前只有单元测试读它；留着作为池占用量的诊断入口。
    #[allow(dead_code)]
    pub(crate) const fn used_count_(&self) -> PoolIndex {
        self.used_count_
    }

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
        let used = (self.used_count_ as usize) * CELL_SIZE;
        let total = (self.cell_count_ as usize) * CELL_SIZE;
        unsafe {
            let data = p.add(Self::THIS_SIZE + used);
            let slice = ptr::slice_from_raw_parts(data, total - used);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    /// 从池尾 bump 出一块满足 `layout` 的内存。
    ///
    /// 分配按 `layout.align()` 向上对齐，对齐跳过的字节也计入 `used_count_`；这是为了让
    /// 对齐要求高于 `CELL_SIZE` 的类型（例如 `u128`）也能落在正确的边界上。
    ///
    /// # Errors
    ///
    /// 剩余空间放不下时返回当前可用的连续字节数，供上层决定是否开新池。
    pub(crate) fn allocate_(&mut self, layout: Layout) -> Result<NonNull<[u8]>, usize> {
        let data_addr = self as *mut Self as usize;
        let cell_addr = data_addr + Self::THIS_SIZE;
        let used_bytes = (self.used_count_ as usize) * CELL_SIZE;
        let free_start = cell_addr + used_bytes;
        let align = layout.align();
        // 向上对齐到 layout 要求的边界（align 是 2 的幂）
        let start = (free_start + align - 1) & !(align - 1);
        let end = start + layout.size();
        let pool_end = cell_addr + (self.cell_count_ as usize) * CELL_SIZE;
        if end > pool_end {
            return Result::Err(pool_end - free_start);
        }
        // 推进 used_count_，把对齐跳过的字节也算成已用
        self.used_count_ = ((end - cell_addr).div_ceil(CELL_SIZE)) as PoolIndex;
        // SAFETY: start..end 落在本池 cell 区内，且 start 已按 layout 对齐
        unsafe {
            Result::Ok(NonNull::new_unchecked(ptr::slice_from_raw_parts_mut(
                start as *mut u8,
                layout.size(),
            )))
        }
    }
}
