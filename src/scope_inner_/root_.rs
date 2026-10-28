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
//!     metas_:       SpinMutex<BPlusTree<ConstTypeId, AnyTypeVTable>>, // 类型级 vtable
//!     alloc_:       &'static (dyn Allocator + Send + Sync),      // 类型已擦除
//! }
//! ```
//!
//! 因而 [`super::ScopeInner`] 与 [`Scope`](crate::Scope) 都可以去掉泛型参数。弱池与强池
//! 都由每个 [`super::ScopeInner`] 自己持有，root 不再保存树级共享弱池。实验分支里域实体
//! 由 `alloc_` 分配一次（锁、ScopeInner 与每域分配器都在同一块内存里），树只登记地址。
//!
//! # 类型信息归属
//!
//! 类型级 vtable 存在 `metas_`，键是强块头部的合成 `ConstTypeId`（见
//! [`StrongChunk::type_id_`]），值是 [`AnyTypeVTable`]。每对象的 `?Sized` metadata **不在**
//! 这里：它逐对象保存在强块的 meta 槽里，因为类型表一条目不可能承载每个切片的长度。
//!
//! `metas_` 有自己的自旋锁。所有查询都返回 [`AnyTypeVTable`] 的**副本**，锁在返回前释放，
//! 因此析构路径拿不到树内引用，也不会在持锁时调用用户 `Drop`。
//!
//! # 单例假设
//!
//! `RootScope` 全进程唯一，固定由 [`DEFAULT_ROOT_SCOPE`] 发布。类型信息查找（注册与析构）
//! 一律走 [`RootScope::default_`]；任何 `Scope` / 实体 / 弱池都**不保存** root 指针。
//! 若以后需要自定义 root，必须先重新讨论这一单例假设。

use core::{
    alloc::{AllocError, Allocator, Layout},
    fmt,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, Ordering},
};

use typeid::ConstTypeId;

use atomex::{AtomexPtr, CmpxchResult};
use atomic_sync::mutex::preemptive::SpinningMutexOwned;
use bplustree_cmp::{BPlusTree, OrdComparer};

use crate::{abs_::TrPredrop, strong_::StrongChunk, weak_::meta_from_raw_, x_deps::atomex};

use super::{DEFAULT_ROOT_SCOPE, PoolIndex, inner_::ScopeInner};

/// B+ 树的节点容量。
const TREE_M: usize = 16;

/// 初始化过程中写入 [`DEFAULT_ROOT_SCOPE`] 的哨兵值。
///
/// 它取发布点自身的地址：既非空，又不可能等于任何真实 `RootScope` 分配地址，因此查找
/// 路径可以据此区分“尚未初始化”“正在初始化”“已发布”三种状态。
fn default_root_sentinel_() -> *mut RootScope {
    &DEFAULT_ROOT_SCOPE as *const AtomicPtr<RootScope> as *const u8 as *const RootScope
        as *mut RootScope
}

pub type MetasBpTree = BPlusTree<TREE_M, OrdComparer<ConstTypeId>, ConstTypeId, AnyTypeVTable>;

/// 类型级析构入口：参数是**数据指针**（thin）与每对象 metadata 裸字。
///
/// 它只析构 `T` 本身，不读取 `StrongChunk` 头部、也不依赖 chunk 是否仍然存活。`?Sized`
/// 的 metadata 由调用方从强块 meta 槽取出后传入。
pub(crate) type FnDropInPlace = unsafe fn(*mut (), *const ());

/// Predrop 入口：参数是强块首址，只用于定位并消费内联存储的 `P` 对象。
///
/// 用户数据本身由 [`FnDropInPlace`] 独立析构；这里的 chunk 依赖仅限 `P` 对象的位置。
pub(crate) type FnPreDrop = unsafe fn(*mut ());

/// 类型级 `Display` / `Debug` 入口；当前尚未接入，保留字段供后续格式化路径使用。
pub(crate) type FnDisplay = unsafe fn(*const (), &mut fmt::Formatter) -> fmt::Result;
pub(crate) type FnDebug = unsafe fn(*const (), &mut fmt::Formatter) -> fmt::Result;

/// 一个强块类型的类型级信息表条目。
///
/// 只保存**类型级**入口，不保存任何逐对象信息：
///
/// - `?Sized` 的每对象 metadata 在强块的 meta 槽里，由调用方传入析构入口；
/// - `P: TrPredrop<T>` 由键类型 `ChunkTy<T, P>` 区分；
/// - `drop_in_place` 只收数据指针，因此用户数据可以先于 `StrongChunk` 析构，不要求
///   vtable 看到 chunk 首址；
/// - `pre_drop` 单独存放，因为 `P` 对象内联在 chunk 里，定位它确实需要 chunk 首址；
/// - 这里没有 `meta` 字段：类型表的一条记录无法承载每个切片的长度。
///
/// 字段都是函数指针 / `Option<fn>`，因此可以 [`Copy`]：查询时复制出来即可，不需要在
/// `metas_` 锁外持有树内引用。
#[derive(Clone, Copy)]
pub(crate) struct AnyTypeVTable {
    pub(super) drop_in_place: FnDropInPlace,
    pub(super) pre_drop: Option<FnPreDrop>,
    pub(super) display: Option<FnDisplay>,
    pub(super) debug: Option<FnDebug>,
}

impl AnyTypeVTable {
    /// 为 `T` / `P` 组合构造类型级 vtable。
    ///
    /// 键必须使用同一个组合的 [`StrongChunk::type_id_`]；本函数只负责填入类型级入口。
    pub(crate) fn of<T: ?Sized, P: TrPredrop<T>>() -> Self {
        AnyTypeVTable {
            drop_in_place: drop_in_place_entry_::<T> as FnDropInPlace,
            pre_drop: if P::IS_ABSENT {
                Option::None
            } else {
                Option::Some(run_predrop_entry_::<T, P> as FnPreDrop)
            },
            display: Option::None,
            debug: Option::None,
        }
    }

    /// 在数据指针上执行类型级析构。
    ///
    /// # Safety
    ///
    /// 调用方必须已经认领销毁、恰好调用一次；`data` 必须指向本条目对应 `T` 的数据区，
    /// `meta` 必须是与该数据匹配的每对象元数据裸字。
    pub(crate) unsafe fn run_drop_(&self, data: *mut (), meta: *const ()) {
        // SAFETY: 由调用方保证 data/meta 与条目类型匹配、且数据尚未析构
        unsafe { (self.drop_in_place)(data, meta) };
    }

    /// 消费强块内联的 Predrop 对象；没有 Predrop 时什么都不做。
    ///
    /// # Safety
    ///
    /// 调用方必须已经认领销毁、恰好调用一次；`chunk` 必须是本条目对应 `T` / `P` 的强块
    /// 首址，且数据尚未析构。
    pub(crate) unsafe fn run_pre_drop_(&self, chunk: *mut ()) {
        if let Option::Some(pre_drop) = self.pre_drop {
            // SAFETY: 由调用方保证 chunk 与条目类型匹配、且 Predrop 对象尚未消费
            unsafe { pre_drop(chunk) };
        }
    }
}

/// 类型级析构入口：由数据指针与每对象 metadata 还原胖指针，再直接析构 `T`。
///
/// # Safety
///
/// `data` / `meta` 必须来自同一个 `T` 的数据区，且数据尚未析构。
unsafe fn drop_in_place_entry_<T: ?Sized>(data: *mut (), meta: *const ()) {
    // SAFETY: 由调用方保证 meta 与 T 匹配
    let meta = unsafe { meta_from_raw_::<T>(meta) };
    let data = ptr::from_raw_parts_mut::<T>(data, meta);
    // SAFETY: 由调用方保证数据尚未析构
    unsafe { ptr::drop_in_place(data) };
}

/// Predrop 入口：只负责定位并消费内联在强块里的 `P` 对象。
///
/// # Safety
///
/// `chunk` 必须是同一个 `T` / `P` 的强块首址，且 Predrop 对象尚未消费。
unsafe fn run_predrop_entry_<T: ?Sized, P: TrPredrop<T>>(chunk: *mut ()) {
    let chunk = chunk.cast::<StrongChunk<T, P>>();
    // SAFETY: 由调用方保证 chunk 类型与生命周期
    unsafe { (*chunk).run_predrop_() };
}

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

    /// 类型级 vtable 表：键是强块头部保存的合成 `ConstTypeId`。
    ///
    /// B+ 树的数据槽在条目存续期间地址稳定，但查询一律复制 [`AnyTypeVTable`] 后立刻
    /// 释放锁，因此强块只保存 id，不保存树内地址。
    metas_: SpinningMutexOwned<MetasBpTree>,

    /// 类型擦除后的分配器；root 自身的元信息 / 实体池都由它分配。
    alloc_: &'static (dyn Allocator + Send + Sync),
}

// SAFETY: `entity_pool_` / `metas_` 各自带自旋锁；`alloc_` 要求 `Send + Sync`；
// root 发布后不再移动。
unsafe impl Send for RootScope {}
// SAFETY: 见上。
unsafe impl Sync for RootScope {}

impl RootScope {
    /// 并发地尝试初始化默认 [`RootScope`]。
    ///
    /// 成功时返回已发布的 root；后到的并发调用会拿到先到者发布的同一个 root，并以
    /// `Err` 返回它。root 全进程唯一，因此本函数不再接收自定义 `AtomicPtr`，固定发布到
    /// [`DEFAULT_ROOT_SCOPE`]。
    ///
    /// # Panics
    ///
    /// 首次分配 root 失败时 panic；root 是进程级单例，没有可恢复的调用方。
    pub(crate) fn try_init_default_root_scope_<A>(
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
            let addr = alloc.allocate(layout)?;
            let root = addr.as_ptr() as *mut RootScopeRepr<TyAlloc>;
            let metas = MetasBpTree::new().map_err(|_| AllocError)?;
            // 逐字段 `ptr::write`，不要在未初始化的 `RootScopeRepr` 上构造 `&mut`。
            // SAFETY: addr 是刚分配、独占且足够大的内存；下面每个字段只写一次。
            unsafe {
                ptr::addr_of_mut!((*root).alloc).write(alloc);
                let alloc_ref: &'static (dyn Allocator + Send + Sync) =
                    &*ptr::addr_of!((*root).alloc);
                let scope = ptr::addr_of_mut!((*root).scope);
                ptr::addr_of_mut!((*scope).metas_).write(SpinningMutexOwned::new_owned(metas));
                ptr::addr_of_mut!((*scope).alloc_).write(alloc_ref);
                ptr::addr_of_mut!((*scope).entity_pool_).write(SpinningMutexOwned::new_owned(
                    ScopeInner::empty_(NonNull::from(alloc_ref), page_size),
                ));
                Result::Ok(NonNull::new_unchecked(scope))
            }
        }

        let atomic_ptr: &'static AtomicPtr<RootScope> = &DEFAULT_ROOT_SCOPE;
        // 初始化中的哨兵值：它是一个非空、但不可能成为真实 root 的地址。
        let impossible_value = default_root_sentinel_();
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
                CmpxchResult::Succ(_) => match alloc_root_scope_(allocator, page_size) {
                    Result::Ok(ptr) => {
                        root_ptr = ptr;
                        break;
                    }
                    Result::Err(_) => {
                        // 初始化失败：把哨兵值换回空，避免其他线程永久自旋。
                        atom_cell.store(ptr::null_mut());
                        panic!("分配 RootScope 失败");
                    }
                },
            }
        }
        atom_cell.store(root_ptr.as_ptr());
        Result::Ok(unsafe { root_ptr.as_ref() })
    }

    /// 取全进程唯一的默认 root；尚未初始化时返回 [`None`]。
    ///
    /// 若另一个线程正在初始化，本函数会自旋等待发布完成；初始化线程本身不会在 root
    /// 可见前调用这里。
    pub(crate) fn default_() -> Option<&'static RootScope> {
        loop {
            let ptr = DEFAULT_ROOT_SCOPE.load(Ordering::Acquire);
            if ptr.is_null() {
                return Option::None;
            }
            if ptr::eq(ptr, default_root_sentinel_()) {
                core::hint::spin_loop();
                continue;
            }
            // SAFETY: root 由 try_init_default_root_scope_ 发布、全进程唯一且永不释放。
            return Option::Some(unsafe { &*ptr });
        }
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

    /// 在 `metas_` 锁内访问类型表。
    ///
    /// 所有公开查询都只在本闭包内读取并**复制** [`AnyTypeVTable`]；闭包返回后锁已释放，
    /// 因此调用方不可能拿到树内引用，也不会在持锁时执行用户 `Drop`。
    fn with_metas_<R>(&self, access: impl FnOnce(&mut MetasBpTree) -> R) -> R {
        let mut session = self.metas_.lock_session();
        let mut guard = match session.lock().wait() {
            Result::Ok(guard) => guard,
            Result::Err(_) => unreachable!("root metas 自旋锁不应失败"),
        };
        access(&mut guard)
    }

    /// 登记一条类型级 vtable，返回表中实际生效的条目。
    ///
    /// 键已存在时不覆盖旧条目，而是返回已有条目（`try_insert` 的冲突分支）。
    #[inline]
    pub(crate) fn insert_meta_(&self, id: ConstTypeId, table: AnyTypeVTable) -> AnyTypeVTable {
        self.with_metas_(|tree| match tree.try_insert(id, table) {
            Result::Ok((_, value)) => *value,
            Result::Err(info) => {
                let (existing, conflict) = info.into_parts();
                let _ = conflict;
                *existing.1
            }
        })
    }

    /// 按合成类型 id 查找 vtable，返回**副本**。
    ///
    /// 返回的 [`AnyTypeVTable`] 不借入 `metas_` 锁；锁在函数返回前已经释放。
    #[inline]
    pub(crate) fn get_meta_(&self, id: ConstTypeId) -> Option<AnyTypeVTable> {
        self.with_metas_(|tree| tree.get(id).map(|(_, value)| *value))
    }

    /// 确保 `id` 有一条 vtable：持锁查重，缺失时调用 `make` 并插入。
    ///
    /// `make` 只应构造纯数据（本 crate 内就是 [`AnyTypeVTable::of`]），不得执行用户
    /// `Drop` / 构造；因此持锁调用它是安全的。并发注册同一 id 时，后到者返回先到者
    /// 插入的条目。
    pub(crate) fn ensure_meta_(
        &self,
        id: ConstTypeId,
        make: impl FnOnce() -> AnyTypeVTable,
    ) -> AnyTypeVTable {
        self.with_metas_(|tree| {
            if let Option::Some((_, value)) = tree.get(id) {
                return *value;
            }
            let table = make();
            match tree.try_insert(id, table) {
                Result::Ok((_, value)) => *value,
                Result::Err(info) => {
                    let (existing, conflict) = info.into_parts();
                    let _ = conflict;
                    *existing.1
                }
            }
        })
    }

    /// 按 `T` / `P` 组合确保 vtable 已登记，返回表中条目。
    ///
    /// 键与强块头部保存的 [`StrongChunk::type_id_`] 完全一致；Predrop 类型 `P` 参与键，
    /// 因此析构入口可以单态化出正确的 `P::pre_drop`。
    pub(crate) fn ensure_chunk_meta_<T: ?Sized, P: TrPredrop<T>>(&self) -> AnyTypeVTable {
        self.ensure_meta_(StrongChunk::<T, P>::type_id_(), || {
            AnyTypeVTable::of::<T, P>()
        })
    }

    /// 按合成类型 id 摘除 vtable。
    #[inline]
    pub(crate) fn remove_meta_(&self, id: ConstTypeId) -> Option<AnyTypeVTable> {
        self.with_metas_(|tree| tree.remove(id))
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

#[cfg(test)]
mod tests_ {
    use super::*;
    use crate::{Scope, scope_inner_::DefaultConfig};
    use std::alloc::Global;

    /// 测试类型表对同一 `ConstTypeId` 只插入一次 vtable。
    /// Test that the type table inserts at most one vtable per `ConstTypeId`.
    ///
    /// - 手段：为一个只在本测试出现的类型连续调用两次 `ensure_meta_`，每次的工厂闭包都
    ///   递增计数器；随后再用 `get_meta_` 查回。
    ///   / How: call `ensure_meta_` twice for a test-local type, incrementing a counter in each
    ///   factory closure, then look the entry up with `get_meta_`.
    /// - 判断：工厂只被调用一次，两次返回同一个析构入口，查表也能取到同一条目。
    ///   / What: the factory runs exactly once, both calls return the same drop entry, and the
    ///   lookup returns the same entry.
    #[test]
    fn ensure_meta_registers_once_per_type_id() {
        struct TableProbe(u64);

        Scope::try_init_root::<DefaultConfig<Global>>();
        let root = RootScope::default_().expect("默认 root 应当已初始化");
        let id = StrongChunk::<TableProbe, ()>::type_id_();
        let mut made = 0usize;

        let first = root.ensure_meta_(id, || {
            made += 1;
            AnyTypeVTable::of::<TableProbe, ()>()
        });
        let second = root.ensure_meta_(id, || {
            made += 1;
            AnyTypeVTable::of::<TableProbe, ()>()
        });

        assert_eq!(made, 1, "同一 id 的工厂只应执行一次");
        assert_eq!(
            first.drop_in_place as usize, second.drop_in_place as usize,
            "两次查询必须返回同一个析构入口"
        );
        let looked_up = root.get_meta_(id).expect("登记过的 id 应当能查到");
        assert_eq!(
            looked_up.drop_in_place as usize, first.drop_in_place as usize,
            "get_meta_ 应返回表中同一条目"
        );
    }

    /// Predrop 类型必须参与合成键；同一个 `T` 配不同 `P` 不能共用 vtable。
    /// The Predrop type must participate in the synthetic key, so one `T` with different
    /// `P` types cannot share a vtable.
    ///
    /// - 手段：分别取 `StrongChunk<u64, ()>` 两次与 `StrongChunk<u64, TableHook>` 一次
    ///   的合成 id。
    ///   / How: take the synthetic id of `StrongChunk<u64, ()>` twice and of
    ///   `StrongChunk<u64, TableHook>` once.
    /// - 判断：同组合的 id 相等，不同 `P` 的 id 不相等。
    ///   / What: ids of the same combination are equal, while ids with different `P` differ.
    #[test]
    fn chunk_type_id_includes_predrop_type() {
        struct TableHook;

        impl TrPredrop<u64> for TableHook {
            fn pre_drop(self, _data: &mut u64) {}
        }

        let plain_a = StrongChunk::<u64, ()>::type_id_();
        let plain_b = StrongChunk::<u64, ()>::type_id_();
        let hooked = StrongChunk::<u64, TableHook>::type_id_();

        assert_eq!(plain_a, plain_b, "同一个 T/P 组合必须得到同一个键");
        assert_ne!(plain_a, hooked, "Predrop 类型不同必须得到不同的键");
    }
    /// 测试类型级 `drop_in_place` 只需要数据指针，不依赖 `StrongChunk` 首址。
    /// Test that the type-level `drop_in_place` only needs a data pointer and does not depend
    /// on the `StrongChunk` address.
    ///
    /// - 手段：用 `ManuallyDrop` 在栈上放一个 `VtDropProbe`，只把它的数据指针与空
    ///   metadata 交给 vtable 的 `run_drop_`，全程不构造 `StrongChunk`。
    ///   / How: place a `VtDropProbe` on the stack in `ManuallyDrop`, pass only its data pointer
    ///   and null metadata to `run_drop_`, without constructing any `StrongChunk`.
    /// - 判断：析构计数恰好为 1，证明数据析构入口与 chunk 地址解耦。
    ///   / What: the drop counter is exactly 1, proving the data destructor is decoupled from
    ///   the chunk address.
    #[test]
    fn vtable_drop_only_needs_data_pointer() {
        use core::mem::ManuallyDrop;

        static VT_DROPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

        struct VtDropProbe;

        impl Drop for VtDropProbe {
            fn drop(&mut self) {
                VT_DROPS.fetch_add(1, Ordering::AcqRel);
            }
        }

        VT_DROPS.store(0, Ordering::Release);
        Scope::try_init_root::<DefaultConfig<Global>>();
        let root = RootScope::default_().expect("默认 root 应当已初始化");
        let vtable = root.ensure_chunk_meta_::<VtDropProbe, ()>();

        let mut value = ManuallyDrop::new(VtDropProbe);
        let data = (&mut *value as *mut VtDropProbe).cast::<()>();
        // SAFETY: data 指向仍存活、尚未析构的 VtDropProbe；metadata 为 Sized 的空值
        unsafe { vtable.run_drop_(data, core::ptr::null()) };

        assert_eq!(VT_DROPS.load(Ordering::Acquire), 1, "数据必须恰好析构一次");
    }
}
