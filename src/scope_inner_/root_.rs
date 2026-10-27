//! 整棵树的根：[`RootScope`] 通过自己的强块池管理所有域实体。
//!
//! # 新设计（重构中）
//!
//! 旧设计把 root 拆成
//!
//! ```text
//! RootScope<A, CELL_SIZE> {
//!     scope_inner_: ScopeInner<CELL_SIZE>,    // root 自己的域状态（与普通域同型）
//!     extension_:   RootExtension<CELL_SIZE>, // root 比普通域多出来的共享内容
//!     allocator_:   A,                        // 具体分配器，永远放最后
//! }
//! ```
//!
//! 并通过"分配器字段反向偏移"从任意域定位 root。新设计不再需要这种布局技巧：
//!
//! ```text
//! RootScope {
//!     entity_pool_: SpinMutex<ScopeInner>,                       // 域实体的强块池
//!     metas_:       BPlusTree<…, usize, *const ()>,              // 强块元信息字（按地址）
//!     alloc_:       &'static (dyn Allocator + Send + Sync),      // 类型已擦除
//! }
//! ```
//!
//! 因而 [`super::ScopeInner`] 与 [`Scope`](crate::Scope) 都可以去掉泛型参数。弱池与强池
//! 都由每个 [`super::ScopeInner`] 自己持有，root 不再保存树级共享弱池。实验分支里域实体
//! 由 `alloc_` 分配一次（锁、ScopeInner 与每域分配器都在同一块内存里），树只登记地址。
//!
//! 旧实现的初始化、共享扩展与反推 root 的代码保留在文件末尾注释中，等 B+ 树操作补齐后
//! 再决定哪些部分需要恢复。

use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::{self, NonNull},
    sync::atomic::AtomicPtr,
};

use atomex::{AtomexPtr, CmpxchResult};
use atomic_sync::mutex::preemptive::SpinningMutexOwned;
use bplustree_cmp::{BPlusTree, OrdComparer};

use crate::{strong_::MetaAddr, x_deps::atomex};

use super::{PoolIndex, inner_::ScopeInner};

/// B+ 树的节点容量。
const TREE_M: usize = 16;

pub type MetasBpTree = BPlusTree<TREE_M, OrdComparer<usize>, usize, *const ()>;

/// 一整棵 Scope 树的根。
///
/// RootScope 的“用户对象”就是各个域实体：它们作为强块存放在 root 自己的
/// `entity_pool_` 里，而不是作为 B+ 树的 value。这样 Scope 持有直接指针即可，
/// 不需要按 key 查树。
#[repr(C)]
pub(crate) struct RootScope {
    /// 域实体的强块池。每个实体是一个 `StrongChunk<ScopeEntity<A>, ()>`，
    /// 弱槽位与强块都由这个域持有，分配 / 冻结由自旋锁串行化。
    entity_pool_: SpinningMutexOwned<ScopeInner>,

    /// 强块元信息字：键是元信息地址，值是一个指针宽度的元数据字。
    ///
    /// 强块只保存地址；元信息内容由本树统一持有。B+ 树的数据槽在条目存续期间地址稳定，
    /// 因此 `insert_meta_` 可以把该地址交给强块。
    metas_: MetasBpTree,

    /// 类型擦除后的分配器；root 自身的元信息 / 实体池都由它分配。
    alloc_: &'static (dyn Allocator + Send + Sync),
}

// SAFETY: `entity_pool_` 自带自旋锁；`metas_` 目前只在初始化 / 后续统一接入的 root 锁
// 路径中访问；`alloc_` 要求 `Send + Sync`；root 发布后不再移动。
unsafe impl Send for RootScope {}
// SAFETY: 见上。
unsafe impl Sync for RootScope {}

impl RootScope {
    /// 并发地尝试初始化一个 RootScope，如果初始化成功返回 `Ok`，否则返回 `Err` 并携带
    /// 被其他调用者初始化的 root。
    ///
    /// # Errors
    ///
    /// 当 `atomic_ptr` 已经被其他线程发布、或第一次分配 root 失败时返回 `Err`。
    // TODO(新设计): root 的 metas_ 还需要自己的锁。
    pub(crate) fn try_init_default_root_scope_<A>(
        atomic_ptr: &'static AtomicPtr<RootScope>,
        page_size: usize,
        allocator: A,
    ) -> Result<&'static RootScope, &'static RootScope>
    where
        A: Allocator + Send + Sync + 'static,
    {
        fn alloc_root_scope_<TyAlloc>(
            alloc: TyAlloc,
            page_size: usize,
        ) -> Result<NonNull<RootScope>, AllocError>
        where
            TyAlloc: 'static + Allocator + Send + Sync,
        {
            #[repr(C)]
            struct RootScopeRepr<TyAlloc> {
                scope: RootScope,
                alloc: TyAlloc,
            }

            let layout = Layout::new::<RootScopeRepr<TyAlloc>>();
            let mut addr = alloc.allocate(layout)?;
            let root = unsafe {
                let p = addr.as_mut();
                let p = &mut p[0] as *mut u8 as *mut RootScopeRepr<TyAlloc>;
                &mut *p
            };
            root.scope.metas_ = MetasBpTree::new().unwrap();
            root.alloc = alloc;
            root.scope.alloc_ = &root.alloc;
            let alloc_ptr: NonNull<dyn Allocator + Send + Sync> = NonNull::from(&root.alloc);
            root.scope.entity_pool_ =
                SpinningMutexOwned::new_owned(ScopeInner::empty_(alloc_ptr, page_size));
            Result::Ok(NonNull::from_mut(&mut root.scope))
        }

        // this is the guarded value telling other racers that, the root scope is
        // being initialized and they should just wait for the final result.
        let impossible_value =
            atomic_ptr as *const _ as *const u8 as *const RootScope as *mut RootScope;
        let atom_cell: AtomexPtr<RootScope, &mut AtomicPtr<RootScope>> =
            AtomexPtr::new(unsafe { &mut *(atomic_ptr as *const _ as *mut _) });
        let expect = |v: *mut RootScope| v.is_null();
        let desire = |_| impossible_value;
        let root_ptr: NonNull<RootScope>;
        loop {
            let res = atom_cell.try_spin_compare_exchange_weak(expect, desire);
            match res {
                CmpxchResult::Fail(_) => continue,
                CmpxchResult::Unexpected(p) => {
                    if ptr::eq(p, impossible_value) {
                        continue;
                    } else {
                        return Result::Ok(unsafe { &*p });
                    }
                }
                CmpxchResult::Succ(_) => {
                    root_ptr = alloc_root_scope_(allocator, page_size).unwrap();
                    break;
                }
            }
        }
        atom_cell.store(root_ptr.as_ptr());
        Result::Ok(unsafe { root_ptr.as_ref() })
    }

    /// 在 root 的实体池上执行一次分配 / 回收操作。
    ///
    /// 只有域实体的创建（以及未来的回收）需要拿这把锁；域内的用户数据操作
    /// 只碰各自的 `ScopeLock`，不会经过这里，因此冲突域只有一个 root 级分配锁。
    pub(crate) fn with_entity_pool_<R>(&self, op: impl FnOnce(&mut ScopeInner) -> R) -> R {
        let mut session = self.entity_pool_.lock_session();
        let mut guard = match session.lock().wait() {
            Result::Ok(guard) => guard,
            Result::Err(_) => unreachable!("root entity_pool 自旋锁不应失败"),
        };
        op(&mut guard)
    }

    /// 登记一条强块元信息，返回元信息在树中的稳定地址。
    ///
    /// 地址已存在时返回 [`None`]，调用方应改用 [`RootScope::get_meta_`]。
    #[inline]
    pub(crate) fn insert_meta_(
        &mut self,
        addr: usize,
        meta: *const (),
    ) -> Option<NonNull<*const ()>> {
        match self.metas_.try_insert(addr, meta) {
            Result::Ok((_, value)) => Option::Some(NonNull::from(value)),
            Result::Err(_) => Option::None,
        }
    }

    /// 按地址查找强块元信息字。
    #[inline]
    pub(crate) fn get_meta_(&self, addr: usize) -> Option<&*const ()> {
        self.metas_.get(addr).map(|(_, meta)| meta)
    }

    /// 查找已有元信息字的稳定地址。
    #[inline]
    pub(crate) fn get_meta_addr_(&self, addr: usize) -> Option<MetaAddr> {
        self.metas_
            .get(addr)
            .map(|(_, meta)| NonNull::from(meta).as_ptr() as MetaAddr)
    }

    /// 尝试注册一个元信息字；已存在时直接返回它原来的地址。
    ///
    /// 这是 `?Sized` 工厂的配套入口：第一个使用该元信息的块负责插入，后续块复用同一个
    /// RootScope B+ 树数据槽，因此强块里的 meta 指针可以稳定共享。
    pub(crate) fn ensure_meta_(&mut self, meta: *const ()) -> MetaAddr {
        let key = meta as usize;
        if let Option::Some(addr) = self.get_meta_addr_(key) {
            return addr;
        }
        self.insert_meta_(key, meta)
            .expect("刚确认该元信息不存在")
            .as_ptr() as MetaAddr
    }

    /// 按地址摘除强块元信息字。
    #[inline]
    pub(crate) fn remove_meta_(&mut self, addr: usize) -> Option<*const ()> {
        self.metas_.remove(addr)
    }

    /// 类型擦除后的分配器。
    #[inline]
    pub(crate) fn allocator_(&self) -> &'static (dyn Allocator + Send + Sync) {
        self.alloc_
    }

    /// 池索引类型（仅为旧代码迁移期间保留的引用）。
    #[allow(dead_code)]
    #[inline]
    pub(crate) const fn pool_index_() -> PoolIndex {
        0
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 旧实现（保留作对照，不要据此恢复布局假设）
//
// pub(crate) type RootScopeRef<const CELL_SIZE: usize> = &'static ScopeInner<CELL_SIZE>;
//
// #[repr(C)]
// pub(crate) struct RootExtension<const CELL_SIZE: usize> {
//     weak_pools_: Option<NonNull<WeakPool<CELL_SIZE, RootExtension<CELL_SIZE>>>>,
//     pre_drop_registry_: PreDropRegistry,
// }
//
// impl<const CELL_SIZE: usize> RootExtension<CELL_SIZE> {
//     pub(crate) fn of_weak_(weak: NonNull<WeakChunk<()>>) -> NonNull<Self> {
//         let pool = WeakPool::<CELL_SIZE, Self>::of_slot_(weak)
//             .unwrap_or_else(|| unreachable!("弱槽位必须落在某个弱池的槽位数组内"));
//         unsafe { pool.as_ref() }.root()
//             .unwrap_or_else(|| unreachable!("弱池必须登记所属 root"))
//     }
// }
//
// impl<A, const CELL_SIZE: usize> RootScope<A, CELL_SIZE> { ... }
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

// ============================================================================
// WIP 提交中的旧 `root_.rs` 结构定义（原样保留，便于对照）：
//
// //! 整棵树的根：共享扩展、内联分配器与一次性初始化，以及由域定位 root。
// use core::{
//     alloc::{Allocator, Layout},
//     mem,
//     ptr::{self, NonNull},
//     sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
// };
// use bplustree_cmp::BPlusTree;
// use crate::{
//     weak_::{PreDropRegistry, WeakSlot, WeakPool},
// };
// use super::{PoolIndex, ScopeInner};
//
// const TREE_M: usize = 16;
// type ScopeKey = usize;
//
// pub(crate) struct RootScope {
//     scopes_: BPlusTree<TREE_M, ScopeKey, NonNull<ScopeInner<>>
// }
//
// 说明：该结构少了 B+ 树所需的 comparer 泛型参数，也没有 root 自身的 ScopeKey 与分配器，
// 因此本文件按新设计补全为可编译版本；上面的原始片段仅作对照。
