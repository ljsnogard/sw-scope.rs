//! 整棵树的根：共享扩展、内联分配器与一次性初始化，以及由域定位 root。
//!
//! root 就是三者按顺序组合出来的一个分配块：
//!
//! ```text
//! RootScope<A, CELL_SIZE> {
//!     scope_inner_: ScopeInner<CELL_SIZE>,    // root 自己的域状态（与普通域同型）
//!     extension_:   RootExtension<CELL_SIZE>, // root 比普通域多出来的共享内容
//!     allocator_:   A,                        // 具体分配器，永远放最后
//! }
//! ```
//!
//! 各域只保存一个 `&'static dyn Allocator`，指向 root 的 `allocator_` 字段。只要分配器字段
//! 之前没有对齐填充，回退
//! `size_of::<ScopeInner>() + size_of::<RootExtension>()` 就回到 root 域，回退
//! `size_of::<RootExtension>()` 就得到扩展。初始化时会断言这一点（见
//! [`RootScope::try_init_default_root_scope_`]）。

use core::{
    alloc::{Allocator, Layout},
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::{
    weak_::{PreDropRegistry, WeakChunk, WeakPool},
    share_marker_::Local,
};

use super::{PoolIndex, ScopeInner};

/// 已经初始化好的 root 域。
pub(crate) type RootScopeRef<const CELL_SIZE: usize> = &'static ScopeInner<CELL_SIZE>;

/// root 比普通 [`ScopeInner`] 多出来的那部分：整棵树共享的内容。
///
/// 它是与分配器类型无关的具体类型，因此擦除侧（`Scope` 这一边）可以直接读它。
#[repr(C)]
pub(crate) struct RootExtension<const CELL_SIZE: usize> {
    /// 整棵树共享的弱槽位池链头。
    weak_pools_: Option<NonNull<WeakPool<CELL_SIZE, RootExtension<CELL_SIZE>>>>,
    /// 整棵树共享的类型级 `PreDrop` 注册表。
    pre_drop_registry_: PreDropRegistry,
}

impl<const CELL_SIZE: usize> RootExtension<CELL_SIZE> {
    /// 整棵树共享的弱槽位池链头。
    #[inline]
    pub(crate) fn weak_pools_head_(
        &self,
    ) -> Option<NonNull<WeakPool<CELL_SIZE, RootExtension<CELL_SIZE>>>> {
        self.weak_pools_
    }

    /// 整棵树共享的类型级 `PreDrop` 注册表。
    #[inline]
    pub(crate) fn pre_drop_registry_(&self) -> &PreDropRegistry {
        &self.pre_drop_registry_
    }

    /// 由对象身份（弱槽位）出发找 root 扩展：**弱槽位 → 弱池 → root**，全程 O(1)。
    ///
    /// 槽位数组紧跟在池头之后，第 `i` 个槽位相对池首的偏移是
    /// `HEADER_BYTES + i * SLOT_SIZE`；`i` 就是槽位里的 `pool_order_`（终生只读），因此
    /// 槽位地址一减就能得到所属弱池；弱池登记着它所属的 root。若手上是强块，先经
    /// [`StrongChunkBase::weak_chunk`](crate::strong_::StrongChunkBase::weak_chunk) 拿到弱槽位
    /// 即可，于是"强块 → 弱槽位 → 弱池 → root"整条路都成立。
    pub(crate) fn of_weak_(weak: NonNull<WeakChunk<()>>) -> NonNull<Self> {
        let pool = match WeakPool::<CELL_SIZE, Self>::of_slot_(weak) {
            Option::Some(pool) => pool,
            Option::None => unreachable!("弱槽位必须落在某个弱池的槽位数组内"),
        };
        // SAFETY: pool 由槽位地址反推得到，且已通过对齐校验
        match unsafe { pool.as_ref() }.root() {
            Option::Some(root) => root,
            Option::None => unreachable!("弱池必须登记所属 root"),
        }
    }
}

/// 一整棵 Scope 树的根：root 域 + 共享扩展 + 内联的具体分配器（放最后）。
///
/// 它**真正持有**弱槽位池链与类型级 `PreDrop` 注册表，因此 [`ScopeInner`] 不再各自复制这
/// 些指针，也不泄漏分配器。
#[repr(C)]
pub(crate) struct RootScope<A, const CELL_SIZE: usize, M = Local> {
    /// root 自己的域状态。
    scope_inner_: ScopeInner<CELL_SIZE>,
    /// root 比普通域多出来的共享内容。
    extension_: RootExtension<CELL_SIZE>,
    /// 本树分配器的具体类型；由 root 终身持有，是最后一个非零大小字段。
    allocator_: A,
    /// 预留的线程模式 marker；当前固定为 `Local`。
    _mode_: PhantomData<M>,
}

impl<A, const CELL_SIZE: usize> RootScope<A, CELL_SIZE, Local>
where
    A: Allocator + 'static,
{
    /// 并发地尝试初始化一个 RootScope，如果初始化成功返回 Ok，否则返回 Err 并携带
    /// 被其他调用者初始化的 Root.
    pub(crate) fn try_init_default_root_scope_(
        atomic_ptr: &'static AtomicPtr<ScopeInner<CELL_SIZE>>,
        page_size: usize,
        allocator: A,
    ) -> Result<RootScopeRef<CELL_SIZE>, RootScopeRef<CELL_SIZE>> {
        // 擦除侧靠固定回退量定位 root，因此分配器字段之前不能有对齐填充。
        assert!(
            mem::offset_of!(Self, allocator_)
                == mem::size_of::<ScopeInner<CELL_SIZE>>()
                    + mem::size_of::<RootExtension<CELL_SIZE>>(),
            "分配器字段与 RootExtension 之间存在对齐填充，无法用固定偏移反推 root",
        );
        let init_cell_count =
            WeakPool::<CELL_SIZE, RootExtension<CELL_SIZE>>::max_count_within_max_size(page_size);
        assert!(
            init_cell_count <= PoolIndex::MAX as usize,
            "init_cell_count({}) <= PoolIndex::MAX({})",
            init_cell_count,
            PoolIndex::MAX,
        );

        // 用这个绝不合法的地址是为了表明抢占中的状态
        let acquired = atomic_ptr as *const AtomicPtr<ScopeInner<CELL_SIZE>>
            as *mut ScopeInner<CELL_SIZE>;
        let expected = ptr::null_mut();
        loop {
            let x = atomic_ptr.compare_exchange_weak(
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
                    return Result::Err(unsafe { &*existing });
                }
            } else {
                break;
            }
        }
        // 这里开始处理抢占成功后的分配内存

        let root_mem = allocator
            .allocate(Layout::new::<Self>())
            .expect("Allocator failed in root scope mem alloc.");
        let root_ptr = root_mem.as_ptr() as *mut u8 as *mut Self;
        // SAFETY: root_ptr 是刚分配、尚无人共享的独占内存；逐个字段初始化
        let inner_ptr = unsafe { ptr::addr_of_mut!((*root_ptr).scope_inner_) };
        let ext_ptr = unsafe { ptr::addr_of_mut!((*root_ptr).extension_) };
        let owner_ptr = unsafe { ptr::addr_of_mut!((*root_ptr).allocator_) };
        let mode_ptr = unsafe { ptr::addr_of_mut!((*root_ptr)._mode_) };
        unsafe {
            // 先把具体分配器移进 root（最后一个非零大小字段），再取擦除引用
            owner_ptr.write(allocator);
            mode_ptr.write(PhantomData);
            let alloc: &'static dyn Allocator = &*owner_ptr;
            // root 域
            inner_ptr.write(ScopeInner {
                flags_: AtomicUsize::new(0usize),
                parent_scope_: Option::None,
                prev_sibling_: Option::None,
                next_sibling_: Option::None,
                first_child_: Option::None,
                last_child_: Option::None,
                pending_head_: Option::None,
                pending_tail_: Option::None,
                pending_next_: Option::None,
                chain_head_: Option::None,
                chain_tail_: Option::None,
                live_head_: Option::None,
                live_tail_: Option::None,
                allocator_: alloc,
            });
            // 共享扩展
            ext_ptr.write(RootExtension {
                weak_pools_: Option::None,
                pre_drop_registry_: PreDropRegistry::new(),
            });
            // 弱槽位池链：由 root 分配，池登记 root 扩展
            let pool = WeakPool::<CELL_SIZE, RootExtension<CELL_SIZE>>::try_new_with_max_size(
                page_size,
                alloc,
            )
            .expect("分配 root 弱池失败");
            (*pool.as_ptr()).set_root(NonNull::new_unchecked(ext_ptr));
            (*ext_ptr).weak_pools_ = Option::Some(pool);
        }
        // 初始化完成后才把真正的地址放入，以表示初始化已完成
        atomic_ptr.store(inner_ptr, Ordering::SeqCst);
        Result::Ok(unsafe { &*atomic_ptr.load(Ordering::Relaxed) })
    }
}

impl<const CELL_SIZE: usize> ScopeInner<CELL_SIZE> {
    /// 本树的 root 扩展（[`RootExtension`]）。
    ///
    /// `allocator_` 指向 root 的 `allocator_` 字段（最后一个字段），它前面依次是
    /// `scope_inner_` 与 `extension_`；初始化时已断言两者之间没有对齐填充，因此从分配器
    /// 指针回退 `size_of::<RootExtension>()` 就得到扩展——既不需要知道分配器类型 `A`，
    /// 也不用修改任何布局。
    ///
    /// root 在首次初始化时一次性分配、终身不回收，因此返回 `'static` 成立。
    pub(crate) fn root_extension_(&self) -> &'static RootExtension<CELL_SIZE> {
        let p = self.allocator_ as *const dyn Allocator as *const u8;
        let back = mem::size_of::<RootExtension<CELL_SIZE>>();
        // SAFETY: 见文档；root 由 `try_init_default_root_scope_` 分配且永不回收
        let ext = unsafe { &*(p.byte_sub(back) as *const RootExtension<CELL_SIZE>) };
        // 自检：弱池里登记的 root 应与反推出来的一致；顺带覆盖
        // "强块 → 弱槽位 → 弱池 → root"这条路径。
        debug_assert!(
            self.live_head_
                .is_none_or(|weak| RootExtension::<CELL_SIZE>::of_weak_(weak) == NonNull::from(ext)),
            "弱池登记的 root 与由 allocator 反推的 root 不一致",
        );
        ext
    }

    /// 整棵树共享的类型级 `PreDrop` 注册表（只读）。
    #[inline]
    pub(crate) fn root_registry_(&self) -> Option<&'static PreDropRegistry> {
        Option::Some(self.root_extension_().pre_drop_registry_())
    }

    /// root 的 `ScopeInner`（待回收名单挂在它上面）。
    #[inline]
    pub(crate) fn root_inner_(&self) -> NonNull<ScopeInner<CELL_SIZE>> {
        let p = self.allocator_ as *const dyn Allocator as *const u8;
        let back =
            mem::size_of::<ScopeInner<CELL_SIZE>>() + mem::size_of::<RootExtension<CELL_SIZE>>();
        // SAFETY: 同 `root_extension_`；root 永不回收
        unsafe { NonNull::new_unchecked(p.byte_sub(back) as *mut ScopeInner<CELL_SIZE>) }
    }
}
