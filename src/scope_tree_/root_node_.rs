//! 整棵树的根：共享扩展、内联分配器与一次性初始化，以及由域定位 root。
//!
//! root 就是三者按顺序组合出来的一个分配块：
//!
//! ```text
//! RootScope<A> {
//!     tree_node_: TreeNodeBase,  // root 自己的域状态（与普通域同型）
//!     root_ext_:  RootNodeExt,  // root 比普通域多出来的共享内容
//!     allocator_: A,            // 具体分配器，永远放最后
//! }
//! ```
//!
//! 各域只保存一个 `&'static dyn Allocator`，指向 root 的 `allocator_` 字段。只要分配器字段
//! 之前没有对齐填充，回退
//! `size_of::<TreeNodeBase>() + size_of::<RootNodeExt>()` 就回到 root 域，回退
//! `size_of::<RootNodeExt>()` 就得到扩展。初始化时会断言这一点（见
//! [`RootScope::try_init_default_root_scope_`]）。

use core::{
    alloc::{Allocator, Layout},
    mem,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::{
    scope_tree_::scope_node_::ScopeNodeExt,
    weak_::{PreDropRegistry, WeakChunk, WeakPool},
};

use super::{PoolIndex, TreeNodeBase};

/// 一整棵 Scope 树的根：root 域 + 共享扩展 + 内联的具体分配器（放最后）。
///
/// 它**真正持有**弱槽位池链与类型级 `PreDrop` 注册表，因此各域不再各自复制这
/// 些指针，也不泄漏分配器。
#[repr(C)]
pub struct RootScope<A> {
    /// root 自己的域状态。
    tree_node_: TreeNodeBase,
    /// root 比普通域多出来的共享内容。
    root_ext_: RootNodeExt,
    /// 本树分配器的具体类型；由 root 终身持有，是最后一个非零大小字段。
    allocator_: A,
}

pub(crate) type RootScopeRef<A> = &'static RootScope<A>;

/// root 比普通域节点多出来的那部分：整棵树共享的内容。
///
/// 它是与分配器类型无关的具体类型，因此擦除侧（`Scope` 这一边）可以直接读它。
#[repr(C)]
/// 根节点中服务于 StrongChunk 和 WeakChunk 相关的功能，刻意剥离 Allocator 后的部分
pub(crate) struct RootNodeExt {
    /// 整棵树共享的弱槽位池链头。
    weak_pools_: Option<NonNull<WeakPool>>,
    /// 整棵树共享的类型级 `PreDrop` 注册表。
    pre_drop_registry_: PreDropRegistry,
}

impl RootNodeExt {
    /// 整棵树共享的弱槽位池链头。
    #[inline]
    pub(crate) fn weak_pools_head_(&self) -> Option<NonNull<WeakPool>> {
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
    /// 槽位地址一减就能得到所属弱池；弱池登记着它所属的 root。
    ///
    /// 若手上是强块，可先经
    /// [`StrongChunk::weak_chunk`](crate::strong_::StrongChunk::weak_chunk) 拿到弱槽位。
    /// **注意**：该路径只在强块已经 `retained()`、因而关联了 `WeakChunk` 时成立；没有弱槽位
    /// 的对象不能走这条反推，也不需要靠它补建身份——补建由 `Owning` / `Sharing` 在
    /// `retained()` 时通过创建它们的 `Scope` 完成。
    pub(crate) fn of_weak_(weak: NonNull<WeakChunk<()>>) -> NonNull<Self> {
        let Option::Some(pool) = WeakPool::of_slot_(weak) else {
            unreachable!("弱槽位必须落在某个弱池的槽位数组内");
        };
        // SAFETY: pool 由槽位地址反推得到，且已通过对齐校验
        let pool = unsafe { pool.as_ref() };
        let Option::Some(root) = pool.root() else {
            unreachable!("弱池必须登记所属 root");
        };
        root
    }
}

impl<A> RootScope<A>
where
    A: Allocator + 'static,
{
    /// 并发地尝试初始化一个 RootScope，如果初始化成功返回 Ok，否则返回 Err 并携带
    /// 被其他调用者初始化的 Root.
    pub(crate) fn try_init_default_root_scope_(
        atomic_ptr: &'static AtomicPtr<Self>,
        page_size: usize,
        allocator: A,
    ) -> Result<RootScopeRef<A>, RootScopeRef<A>> {
        // 擦除侧靠固定回退量定位 root，因此分配器字段之前不能有对齐填充。
        #[allow(unused)]
        if true {
            let offset_alloc = mem::offset_of!(Self, allocator_);
            let length_inner = mem::size_of::<TreeNodeBase>();
            let root_ext_len = mem::size_of::<RootNodeExt>();
            debug_assert!(
                offset_alloc == length_inner + root_ext_len,
                "分配器字段与 RootExtension 之间存在对齐填充，无法用固定偏移反推 root",
            );
        }
        let init_cell_count = WeakPool::max_count_within_max_size(page_size);
        assert!(
            init_cell_count <= PoolIndex::MAX as usize,
            "init_cell_count({}) <= PoolIndex::MAX({})",
            init_cell_count,
            PoolIndex::MAX,
        );

        // 用这个绝不合法的地址是为了表明抢占中的状态
        let acquired = atomic_ptr as *const AtomicPtr<Self>
            as *mut Self;
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
        let base_ptr = unsafe { ptr::addr_of_mut!((*root_ptr).tree_node_) };
        let ext_ptr = unsafe { ptr::addr_of_mut!((*root_ptr).root_ext_) };
        let owner_ptr = unsafe { ptr::addr_of_mut!((*root_ptr).allocator_) };
        unsafe {
            // 先把具体分配器移进 root（最后一个非零大小字段），再取擦除引用
            owner_ptr.write(allocator);
            let alloc: &'static dyn Allocator = &*owner_ptr;
            // root 域
            base_ptr.write(TreeNodeBase::new(Option::None));
            // 共享扩展
            ext_ptr.write(RootNodeExt {
                weak_pools_: Option::None,
                pre_drop_registry_: PreDropRegistry::new(),
            });
            // 弱槽位池链：由 root 分配，池登记 root 扩展
            let pool = WeakPool::try_new_with_max_size(
                page_size,
                alloc,
            )
            .expect("分配 root 弱池失败");
            (*pool.as_ptr()).set_root(NonNull::new_unchecked(ext_ptr));
            (*ext_ptr).weak_pools_ = Option::Some(pool);
        }
        // 初始化完成后才把真正的地址放入，以表示初始化已完成
        atomic_ptr.store(root_ptr, Ordering::SeqCst);
        Result::Ok(unsafe { &*atomic_ptr.load(Ordering::Relaxed) })
    }

    /// 整棵树共享的弱槽位池链头。
    #[inline]
    pub(crate) fn weak_pools_head_(&self) -> Option<NonNull<WeakPool>> {
        self.root_ext_.weak_pools_
    }

    /// 整棵树共享的类型级 `PreDrop` 注册表。
    #[inline]
    pub(crate) fn pre_drop_registry_(&self) -> &PreDropRegistry {
        &self.root_ext_.pre_drop_registry_
    }

    /// 由对象身份（弱槽位）出发找 root 扩展：**弱槽位 → 弱池 → root**，全程 O(1)。
    ///
    /// 槽位数组紧跟在池头之后，第 `i` 个槽位相对池首的偏移是
    /// `HEADER_BYTES + i * SLOT_SIZE`；`i` 就是槽位里的 `pool_order_`（终生只读），因此
    /// 槽位地址一减就能得到所属弱池；弱池登记着它所属的 root。
    ///
    /// 若手上是强块，可先经
    /// [`StrongChunk::weak_chunk`](crate::strong_::StrongChunk::weak_chunk) 拿到弱槽位。
    /// **注意**：该路径只在强块已经 `retained()`、因而关联了 `WeakChunk` 时成立；没有弱槽位
    /// 的对象不能走这条反推，也不需要靠它补建身份——补建由 `Owning` / `Sharing` 在
    /// `retained()` 时通过创建它们的 `Scope` 完成。
    pub(crate) fn of_weak_(weak: NonNull<WeakChunk<()>>) -> NonNull<Self> {
        let Option::Some(pool) = WeakPool::of_slot_(weak) else {
            unreachable!("弱槽位必须落在某个弱池的槽位数组内");
        };
        // SAFETY: pool 由槽位地址反推得到，且已通过对齐校验
        let pool = unsafe { pool.as_ref() };
        let Option::Some(root) = pool.root() else {
            unreachable!("弱池必须登记所属 root");
        };
        root
    }
}

impl<A> AsRef<TreeNodeBase> for RootScope<A>
where
    A: Allocator,
{
    fn as_ref(&self) -> &TreeNodeBase {
        &self.tree_node_
    }
}
