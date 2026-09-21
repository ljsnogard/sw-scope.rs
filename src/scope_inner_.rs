use core::{
    alloc::{AllocError, Allocator, Layout},
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::weak_::WeakChunk;

pub(crate) const DEFAULT_CELL_SIZE: usize = mem::size_of::<usize>();
pub(crate) const DEFAULT_PAGE_SIZE: usize = 4 * 4096usize;

pub type PoolIndex = u16;

pub(crate) static DEFAULT_ROOT_SCOPE: AtomicPtr<ScopeInner<DEFAULT_CELL_SIZE>> = AtomicPtr::new(ptr::null_mut());

#[repr(C)]
pub(crate) struct ScopeInner<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> {
    /// 子域数量
    children_num_: AtomicUsize,
    /// 父域指针
    parent_scope_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
    /// 指向 Scope 所持有的第一块内存池
    chain_head_: Option<NonNull<ChainedPool<CELL_SIZE>>>,
    /// 指向 Scope 所持有的最新一块内存池
    chain_tail_: Option<NonNull<ChainedPool<CELL_SIZE>>>,
    /// 指向本 Scope 所有内存池的分配器，也就是 RootScope 的分配器
    /// 也可用于计算 RootScope 的地址
    allocator_: &'static dyn Allocator,
}

impl<const CELL_SIZE: usize> ScopeInner<CELL_SIZE> {
    pub const fn root_scope(&self) -> NonNull<ScopeInner<CELL_SIZE>> {
        let p = self.allocator_ as *const dyn Allocator as *const u8;
        unsafe {
            let addr = p.byte_sub(mem::size_of::<Self>());
            let inner = addr as *const Self;
            NonNull::new_unchecked(inner as *mut Self)
        }
    }

    /// 只统计最新的一个内存链中可分配空间大小，因为其他空间默认不会被提前释放，
    /// 因此不必统计。`Scope` 或者说所有 Arena 的使用者就是为了一次性兜底释放，
    /// 才会选用 Arena 而不是直接用智能指针。
    pub fn free_size(&self) -> usize {
        let Option::Some(f) = self.chain_tail_ else {
            return 0usize;
        };
        let b = unsafe { f.as_ref() };
        b.free_addr_().len()
    }

    pub fn allocate(
        &mut self,
        layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        todo!("[ScopeInner::allocate] not implemented.")
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 内嵌式内存链块，链块的头部若干(16)字节就是自身。内部存储的就是 StrongChunk。
/// 由于 PoolIndex 只有 16 位，多数情况下 CELL_SIZE 是 8，于是一个 ChainedPool
/// 的大小通常不会大于 2^(16 + 3) = 192 K Bytes。
#[repr(C)]
struct ChainedPool<const CELL_SIZE: usize> {
    /// 上一个内存链块地址
    prev_pool_: Option<NonNull<ChainedPool<CELL_SIZE>>>,
    /// 荷载可用的 cell 的数量
    cell_count_: PoolIndex,
    /// 分配已用的 cell 的数量，由于 ChainedPool 的分配方式总是从前到后。且
    /// 总是一次性全体回收。所以根据 used_count 可以计算出第一个空闲位置的地址。
    used_count_: PoolIndex,
}

impl<const CELL_SIZE: usize> ChainedPool<CELL_SIZE> {
    const THIS_SIZE: usize = mem::size_of::<Self>();

    const fn memory_(&self) -> NonNull<[u8]> {
        let p = self as *const Self as *const u8;
        unsafe {
            let data = p.add(ChainedPool::<CELL_SIZE>::THIS_SIZE);
            let len = (self.cell_count_ as usize) * CELL_SIZE;
            let slice = ptr::slice_from_raw_parts(data, len);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    const fn free_addr_(&self) -> NonNull<[u8]> {
        let p = self as *const Self as *const u8;
        let offset = (self.used_count_ as usize) * CELL_SIZE;
        unsafe {
            let data = p.add(ChainedPool::<CELL_SIZE>::THIS_SIZE + offset);
            let len = (self.cell_count_ as usize) * CELL_SIZE;
            let slice = ptr::slice_from_raw_parts(data, len);
            NonNull::new_unchecked(slice as *mut [u8])
        }
    }

    /// 尝试分配，若容量不足，返回最大能分配的长度
    fn allocate_(
        &mut self,
        layout: Layout,
    ) -> Result<NonNull<[u8]>, usize> {
        todo!("[ChainedBlock::allocate_] not implemented")
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 一个专门用于存储 WeakChunk 的内存池。
/// 由于内部固定使用 PoolIndex (u16) 作为索引，因此其最大内存占用量一般在
/// 64K * size_of::<WeakChunk<()>>() , 以 WeakChunk 大小为 16B 计算，约 1MB 左右。
pub(crate) struct WeakChunkPool<const CELL_SIZE: usize> {
    // 最多可以存储多少个 WeakChunk, 同时也决定了 bit array 有多长
    capacity_: PoolIndex,
    // 已经分配出去的 WeakChunk 数量，注意 WeakChunkPool 的分配顺序是不可知的
    used_length_: PoolIndex,
    // 存储 cell 地址的偏移量（按 WeakChunk 个数计算）
    cell_offset_: PoolIndex,
    // 最近一个闲置 WeakChunk 的索引（当 used_length_ == capacity 时失效）
    latest_free_: PoolIndex,

    prev_: Option<NonNull<WeakChunkPool<CELL_SIZE>>>,
    next_: Option<NonNull<WeakChunkPool<CELL_SIZE>>>,
    root_: Option<NonNull<ScopeInner<CELL_SIZE>>>,
}

impl<const CELL_SIZE: usize> WeakChunkPool<CELL_SIZE> {
    /// 根据目标要容纳的 WeakChunk 数量，计算最小内存占用量
    pub fn min_size_for_max_count(count: PoolIndex) -> usize {
        todo!()
    }

    /// 根据目标最大的内存占用量，计算最大能容纳多少个 WeakChunk
    pub fn max_count_within_max_size(size: usize) -> usize {
        todo!()
    }

    pub fn try_new_with_cell_count(
        cell_count: PoolIndex,
        alloc: &'static dyn Allocator,
    ) -> Result<NonNull<Self>, AllocError> {
        todo!()
    }

    pub fn try_new_with_max_size(
        max_size: usize,
        alloc: &'static dyn Allocator,
    ) -> Result<NonNull<Self>, AllocError> {
        todo!()
    }

    pub const fn chunks(&mut self) -> NonNull<[WeakChunk<()>]> {
        let this = self as *mut Self as *mut u8;
        let offset = self.cell_offset_ as usize * mem::size_of::<WeakChunk<()>>();
        let length = self.capacity_ as usize;
        let chunks = unsafe { this.byte_add(offset) as *mut WeakChunk<()> };
        let ptr = ptr::slice_from_raw_parts_mut(chunks, length);
        unsafe { NonNull::new_unchecked(ptr) }
    }

    pub fn allocate(&mut self) -> Option<PoolIndex> {
        todo!()
    }

    pub fn deallocate(&mut self, index: PoolIndex) -> bool {
        todo!()
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

pub(crate) type RootScopeRef<const CELL_SIZE: usize>
    = &'static ScopeInner<CELL_SIZE>;

#[repr(C)]
pub(crate) struct RootScope<A, const CELL_SIZE: usize>
where
    A: Sized + Allocator,
{
    scope_inner_: ScopeInner<CELL_SIZE>,
    allocator_: A,
    weak_chunks_: Option<NonNull<WeakChunkPool<CELL_SIZE>>>,
}

impl<A, const CELL_SIZE: usize> RootScope<A, CELL_SIZE>
where
    A: Sized + Allocator,
{
    /// 并发地尝试初始化一个 RootScope，如果初始化成功返回 Ok，否则返回 Err 并携带
    /// 被其他调用者初始化的 Root.
    pub(crate) fn try_init_default_root_scope_(
        root_ptr_ref: &'static AtomicPtr<ScopeInner<CELL_SIZE>>,
        page_size: usize,
        allocator: A,
    ) -> Result<RootScopeRef<CELL_SIZE>, RootScopeRef<CELL_SIZE>>
    where
        A: 'static,
    {
        let init_cell_count = WeakChunkPool::<CELL_SIZE>
            ::max_count_within_max_size(page_size);
        assert!(
            init_cell_count <= PoolIndex::MAX as usize,
            "init_cell_count({}) <= PoolIndex::MAX({})",
            init_cell_count, PoolIndex::MAX,
        );
        let init_cell_count = init_cell_count as PoolIndex;

        // 用这个绝不合法的地址是为了表明抢占中的状态
        let acquired = root_ptr_ref as *const AtomicPtr<_>
            as *mut AtomicPtr<ScopeInner<CELL_SIZE>>
            as *mut ScopeInner<CELL_SIZE>;
        let expected = ptr::null_mut();
        loop {
            let x = root_ptr_ref.compare_exchange_weak(
                expected,
                acquired,
                Ordering::Release,
                Ordering::Relaxed,
            );
            if let Result::Err(existing) = x {
                // 抢占中或者未初始化，都必须忙等以得到初始化的结果
                if existing == acquired || existing.is_null() {
                    continue;
                } else {
                    return Result::Err(unsafe { &* existing });
                }
            } else {
                break;
            }
        }
        // 这里开始处理抢占成功后的分配内存

        let mut addr = allocator
            .allocate(Layout::new::<RootScope<A, CELL_SIZE>>())
            .expect("Allocator failed in mem alloc.");
        let root_scope_ptr = unsafe {
            let slice_mut = addr.as_mut();
            &mut slice_mut[0] as *mut u8 as *mut Self
        };
        let root_scope_mut: &'static mut Self = unsafe {
            &mut * root_scope_ptr
        };
        // 这里开始手动初始化 root scope
        root_scope_mut.allocator_ = allocator;
        let alloc: &'static dyn Allocator = &mut root_scope_mut.allocator_;
        root_scope_mut.scope_inner_ = ScopeInner {
            children_num_: AtomicUsize::new(0usize),
            parent_scope_: Option::None,
            chain_head_: Option::None,
            chain_tail_: Option::None,
            allocator_: alloc,
        };
        let x = WeakChunkPool::try_new_with_max_size(page_size, alloc)
            .expect("");
        root_scope_mut.weak_chunks_ = Option::Some(x);
        // 初始化完成后才把真正的地址放入，以表示初始化已完成
        root_ptr_ref.store(
            &mut root_scope_mut.scope_inner_,
            Ordering::SeqCst,
        );
        Result::Ok(unsafe {
            let p = root_ptr_ref.load(Ordering::Relaxed);
            &*p
        })
    }

}
