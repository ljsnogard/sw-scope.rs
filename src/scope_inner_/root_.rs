//! 整棵树的根：[`RootScope`] 通过一棵 B+ 树管理所有 [`ScopeInner`]。
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
//!     scopes_: BPlusTree<…, NonNull<ScopeInner>, ()>,          // 所有域（按地址）
//!     metas_:  BPlusTree<…, usize, *const ()>,                 // 强块元信息字（按地址）
//!     inner_:  ScopeInner,                                    // 分配 ScopeInnerWithCfg 的域
//!     alloc_:  &'static dyn Allocator,                        // 类型已擦除
//! }
//! ```
//!
//! 因而 [`super::ScopeInner`] 与 [`Scope`](crate::Scope) 都可以去掉泛型参数。弱池与强池
//! 都由每个 [`super::ScopeInner`] 自己持有，root 不再保存树级共享弱池。
//!
//! 旧实现的初始化、共享扩展与反推 root 的代码保留在文件末尾注释中，等 B+ 树操作补齐后
//! 再决定哪些部分需要恢复。

use core::{
    alloc::{AllocError, Allocator, Layout},
    cmp::Ordering,
    ptr::{self, NonNull},
    sync::atomic::AtomicPtr,
};

use atomex::{AtomexPtr, CmpxchResult};
use bplustree_cmp::{BPlusTree, OrdComparer, TrComparer};

use crate::{
    strong_::MetaAddr,
    x_deps::atomex,
};

use super::{PoolIndex, inner_::ScopeInner};

/// B+ 树的节点容量。
const TREE_M: usize = 16;

pub type ScopesBpTree = BPlusTree<TREE_M, ScopePtrComparer, NonNull<ScopeInner>, ()>;
pub type MetasBpTree = BPlusTree<TREE_M, OrdComparer<usize>, usize, *const ()>;

/// 一整棵 Scope 树的根。
///
/// 它真正持有两棵 B+ 树与类型擦除后的分配器：所有 `ScopeInner` 按地址登记在 `scopes_` 里，
/// 强块元信息字按地址登记在 `metas_` 里。弱池 / 强池都由各域自己持有。
#[repr(C)]
pub(crate) struct RootScope {
    scopes_: ScopesBpTree,

    /// 强块元信息字：键是元信息地址，值是一个指针宽度的元数据字。
    ///
    /// 强块只保存地址；元信息内容由本树统一持有。B+ 树的数据槽在条目存续期间地址稳定，
    /// 因此 `insert_meta_` 可以把该地址交给强块。
    metas_: MetasBpTree,

    /// 专用于分配任意 ScopeInnerWithAlloc 的 Scope。
    inner_: ScopeInner,

    alloc_: &'static dyn Allocator,
}

impl RootScope {
    /// 并发地尝试初始化一个 RootScope，如果初始化成功返回 `Ok`，否则返回 `Err` 并携带
    /// 被其他调用者初始化的 root。
    ///
    /// # Errors
    ///
    /// 当 `atomic_ptr` 已经被其他线程发布、或第一次分配 root 失败时返回 `Err`。
    // TODO(新设计): 基于 B+ 树与 `&'static dyn Allocator` 重写初始化流程。
    pub(crate) fn try_init_default_root_scope_<A>(
        atomic_ptr: &'static AtomicPtr<RootScope>,
        _page_size: usize,
        allocator: A,
    ) -> Result<&'static RootScope, &'static RootScope>
    where
        A: Allocator + 'static,
    {
        fn alloc_root_scope_<TyAlloc>(
            alloc: TyAlloc,
        ) -> Result<NonNull<RootScope>, AllocError>
        where
            TyAlloc: 'static + Allocator,
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
            root.scope.scopes_ = ScopesBpTree::with_comparer(ScopePtrComparer).unwrap();
            root.scope.metas_ = MetasBpTree::new().unwrap();
            root.scope.inner_ = ScopeInner::root_();
            root.alloc = alloc;
            root.scope.alloc_ = &root.alloc;
            Result::Ok(NonNull::from_mut(&mut root.scope))
        }

        // this is the guarded value telling other racers that, the root scope is
        // being initialized and they should just wait for the final result.
        let impossible_value = atomic_ptr
            as *const _
            as *const u8
            as *const RootScope
            as *mut RootScope;
        let atom_cell: AtomexPtr<RootScope, &mut AtomicPtr<RootScope>,> = AtomexPtr::new(unsafe {
            &mut *(atomic_ptr as *const _ as *mut _)
        });
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
                    root_ptr = alloc_root_scope_(allocator).unwrap();
                    break;
                }
            }
        };
        atom_cell.store(root_ptr.as_ptr());
        Result::Ok(unsafe { root_ptr.as_ref() })
    }

    /// 根 B+ 树的只读视图。
    #[inline]
    pub(crate) const fn scopes_(&self) -> &ScopesBpTree {
        &self.scopes_
    }

    /// 根 B+ 树的可变视图；登记 / 移除域时使用。
    #[inline]
    pub(crate) fn scopes_mut_(&mut self) -> &mut ScopesBpTree {
        &mut self.scopes_
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

    // /// 把一个域登记进 root 的 B+ 树；返回被覆盖的旧值（正常情况下为 `None`）。
    // ///
    // /// 这是新设计取代父子 / 兄弟链的核心入口：域之间的归属关系不再由域内指针表达，
    // /// 而是由 root 的树统一记录。
    // #[inline]
    // pub(crate) fn insert_scope_(
    //     &mut self,
    //     key: ScopeKey,
    //     inner: NonNull<ScopeInner>,
    // ) -> Option<NonNull<ScopeInner>> {
    //     self.scopes_.insert(key, inner)
    // }

    // /// 按 [`ScopeKey`] 查找域。
    // #[inline]
    // pub(crate) fn get_scope_(&self, key: ScopeKey) -> Option<NonNull<ScopeInner>> {
    //     self.scopes_.get(key).map(|(_, inner)| *inner)
    // }

    // /// 从 root 的 B+ 树中摘除一个域。
    // #[inline]
    // pub(crate) fn remove_scope_(&mut self, key: ScopeKey) -> Option<NonNull<ScopeInner>> {
    //     self.scopes_.remove(key)
    // }


    /// 类型擦除后的分配器。
    #[inline]
    pub(crate) fn allocator_(&self) -> &'static dyn Allocator {
        self.alloc_
    }

    /// 池索引类型（仅为旧代码迁移期间保留的引用）。
    #[allow(dead_code)]
    #[inline]
    pub(crate) const fn pool_index_() -> PoolIndex {
        0
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScopePtrComparer;

impl TrComparer<NonNull<ScopeInner>> for ScopePtrComparer {
    fn compare(
        &self,
        a: &NonNull<ScopeInner>,
        b: &NonNull<ScopeInner>,
    ) -> Ordering {
        let a = a.as_ptr() as usize;
        let b = b.as_ptr() as usize;
        <usize as Ord>::cmp(&a, &b)
    }
}

impl ScopeInner {
    fn root_() -> Self {
        todo!()
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
