use core::{
    alloc::{AllocError, Allocator, Layout},
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::weak_::WeakChunk;

pub(crate) const DEFAULT_CELL_SIZE: usize = mem::size_of::<usize>();

pub(crate) type PoolIndex = u16;

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
    pub const fn root_scope(&self) -> &ScopeInner<CELL_SIZE> {
        let p = self.allocator_ as *const dyn Allocator as *const u8;
        unsafe {
            let addr = p.byte_sub(mem::size_of::<Self>());
            let inner = addr as *const Self;
            &*inner
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
    /// 所有可用的 cell 的数量
    cell_count_: PoolIndex,
    /// 分配载荷已用的 cell 的数量，由于 ChainedPool 的分配方式总是从前到后。且
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
/// 内部包含用于指示可用内存块的 bit array，以及指示前趋和后继
pub(crate) struct WeakChunkPool<const CELL_SIZE: usize> {
    /// 此 WeakChunkPool 的内存大小，用于将自身还原成 NonNull<[u8]>
    msize_: usize,
}

impl<const CELL_SIZE: usize> WeakChunkPool<CELL_SIZE> {
    pub unsafe fn try_init(p: NonNull<[u8]>) -> Option<&'static mut Self> {
        todo!()
    }

    pub fn chunks(&self) -> NonNull<[WeakChunk<()>]> {
        todo!()
    }

    pub fn allocate(&mut self) -> Option<u16> {
        todo!()
    }

    pub fn deallocate(&mut self, key: u16) -> bool {
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
}

impl<A, const CELL_SIZE: usize> RootScope<A, CELL_SIZE>
where
    A: Sized + Allocator,
{
    pub(crate) fn try_init_default_root_scope_(
        root_ptr_ref: &'static AtomicPtr<ScopeInner<CELL_SIZE>>,
        init_cell_count: u32,
        allocator: A,
    ) -> Result<RootScopeRef<CELL_SIZE>, RootScopeRef<CELL_SIZE>>
    where
        A: 'static + Allocator,
    {
        let root_scope_size = mem::size_of::<RootScope<A, CELL_SIZE>>();

        assert!(root_scope_size.is_multiple_of(CELL_SIZE));
        let root_scope_cell_count = root_scope_size / CELL_SIZE;
        let init_cell_count = init_cell_count as usize;

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
        let layout = Layout
            ::array
            ::<[u8; CELL_SIZE]>(init_cell_count + root_scope_cell_count)
            .expect("Unsupported Layout");
        let mut addr = allocator
            .allocate(layout)
            .expect("Allocator failed in mem alloc.");
        {
            let mem_size = unsafe { addr.as_ref().len() };
            assert!(mem_size > root_scope_cell_count * CELL_SIZE);
        }
        let root_scope_ptr = unsafe {
            let slice_mut = addr.as_mut();
            &mut slice_mut[0] as *mut u8 as *mut Self
        };
        let allocator_mut: &'static dyn Allocator = unsafe {
            let scope_mut: &'static mut Self = &mut *root_scope_ptr;
            scope_mut.allocator_ = allocator;
            &mut scope_mut.allocator_
        };
        let root_scope_mut = unsafe { &mut * root_scope_ptr };
        root_scope_mut.scope_inner_ = ScopeInner {
            children_num_: AtomicUsize::new(0usize),
            parent_scope_: Option::None,
            chain_head_: Option::None,
            chain_tail_: Option::None,
            allocator_: allocator_mut,
        };
        // 这里把抢占成功后的地址放入，以表示初始化已完成
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
