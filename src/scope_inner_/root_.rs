//! 整棵树的根：[`RootScope`] 通过自己的实体池管理所有域，并用 per-root 的类型表完成
//! 类型擦除析构。
//!
//! # 多实例化
//!
//! `RootScope` **不再**是进程级单例，也没有任何隐式 static 发布点。调用方为每个 root
//! 提供一个静态发布槽位：
//!
//! ```text
//! &'static mut AtomicPtr<RootScope>   // 调用方提供，root 保存其共享形式用于最终清空
//! ```
//!
//! 因此进程内可以同时存在任意多个互相独立的树：每个 root 有自己的实体池、自己的类型表
//! （`metas_`）、自己的分配器与自己的关闭状态。类型表不做全局注册，同一个
//! [`ConstTypeId`] 在不同 root 里各自登记，互不串表。
//!
//! # 生命周期
//!
//! `RootScope` **不实现 `Drop`**：root 的内存由它自己的分配器分配，而“整棵树结束”是
//! 一个显式信号，不能由某个恰好析构的 Rust 值来隐式触发。调用方用
//! [`RootScope::close`] 发出关闭信号：
//!
//! 1. `close` 只把 `flags_` 的 closing 位置真（并在实体池锁内与“创建新 Scope”串行化），因此所有
//!    域都拒绝新的放入；
//! 2. 真正的最终清理由**最后一个完成清空的 Scope 所在线程**负责：它在
//!    [`ScopeInner::try_retire_`](super::ScopeInner) 里把 `live_scopes_` 减到 0 之后，
//!    返回 root 指针给调用方，由调用方在完全退出域实体后再执行
//!    [`RootScope::finalize_`]；
//! 3. 如果调用 `close` 时已经没有存活域，`close` 就在当前线程完成最终清理。
//!
//! 最终清理顺序固定为：把 root 实体池当作普通域清盘，按弱槽位链逐个析构
//! `ScopeEntity<A>`（数据早已由各域的 `close`
//! 清空）→ 回收 root 实体池的弱池 / 强池 → `drop` per-root 类型表 → 清空发布槽位 →
//! 释放 root 存储。
//!
//! # `close` 的 unsafe 契约
//!
//! root close 之后仍然存活的 `Scope` / `Retain` 属于调用方责任：
//!
//! - 这些句柄可能指向已被释放的实体 / 弱池；
//! - 最终清理发生时，root 内存本身会被归还给分配器；
//! - [`RootScope::close`] 因此标记为 `unsafe`，调用方必须保证关闭之后不再使用任何
//!   由该 root 创建的 `Scope` / `Retain` / `Owning` / `Sharing`。
//!
//! # 类型信息归属
//!
//! 类型级 vtable 存在 `metas_`，键是强块头部的合成 `ConstTypeId`（见
//! [`StrongChunk::type_id_`]），值是 [`AnyTypeVTable`]。每对象的 `?Sized` metadata
//! **不在**这里：它逐对象保存在强块的 meta 槽里。
//!
//! 析构路径的查找链路是
//! `WeakSlot -> WeakPool.scope_ -> ScopeInner.root_ -> RootScope.metas_`；
//! 不再有任何 `RootScope::default_()`。

use core::{
    alloc::{AllocError, Allocator, Layout},
    fmt,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering},
};

use typeid::ConstTypeId;

use atomex::{AtomicFlags, CmpxchResult};
use atomic_sync::mutex::preemptive::SpinningMutexOwned;
use bplustree_cmp::{BPlusTree, OrdComparer};

use crate::{
    abs_::TrPredrop,
    scope_inner_::{PoolIndex, ScopeEntity, ScopeInner},
    strong_::StrongChunk,
    weak_::meta_from_raw_,
    x_deps::atomex,
};

/// B+ 树的节点容量。
const TREE_M: usize = 16;

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
///
/// vtable 本身**不携带 root**：它是纯类型级入口，可以由任意 root 的表共享同一份值；
/// “用哪张表”完全由查表时的 root 决定。
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

/// root 状态字：三个只读/一次性状态位合并在同一个原子字里。
///
/// 合并前它们是三个独立的 `AtomicBool`；合并后 root 头部的原子状态只需一条缓存行，
/// `close`、最终清理选举与租约申请共享同一套 `SeqCst` 顺序。
type RootFlags = AtomicFlags<u32, AtomicU32>;

/// 已经收到 root close 信号。
const FLAG_CLOSING: u32 = 1 << 0;
/// 某个线程已经赢得最终清理权。
const FLAG_FINALIZE_ELECTED: u32 = 1 << 1;
/// 最终清理已经开始，新的 root 租约一律失败。
const FLAG_FINALIZING: u32 = 1 << 2;
/// 两个“收尾终态”位；置位后不能再参与选举。
const FLAG_FINALIZE_TERMINAL: u32 = FLAG_FINALIZE_ELECTED | FLAG_FINALIZING;

/// root 的真实存储：`RootScope` 在前，具体分配器在尾部。
///
/// 这样 `RootScope.alloc_` 可以指向同一块内存里的分配器，最终清理时再用它释放整块存储。
#[repr(C)]
struct RootScopeStorage<A> {
    root: RootScope,
    alloc: A,
}

/// 一次 root 使用租约。
///
/// 只要还有租约存在，`RootScope::finalize_` 就不会释放 root 存储 / 实体池；新租约在
/// `finalizing_` 置真之后一律失败。
pub(crate) struct RootLease {
    root_: NonNull<RootScope>,
}

impl RootLease {
    /// 申请一次 root 使用租约；finalizing 已经开始时返回 [`None`]。
    ///
    /// # 安全前提
    ///
    /// 调用方必须在申请时持有某个“让 root 仍然存活”的引用（`Scope` 句柄、`Retain`
    /// 槽位、或 `&RootScope` 本身），否则读 `finalizing_` 就已经悬空。
    pub(crate) fn enter_(root: NonNull<RootScope>) -> Option<Self> {
        // SAFETY: 调用方保证 root 在申请期间仍然存活
        let root_ref = unsafe { root.as_ref() };
        if root_ref.is_finalizing_() {
            return Option::None;
        }
        root_ref.active_.fetch_add(1, Ordering::AcqRel);
        if root_ref.is_finalizing_() {
            // 与 finalize_ 的置位竞态：撤销计数后放弃，不再触碰树内内存。
            root_ref.active_.fetch_sub(1, Ordering::AcqRel);
            return Option::None;
        }
        Option::Some(RootLease { root_: root })
    }
}

impl Drop for RootLease {
    fn drop(&mut self) {
        // SAFETY: finalize_ 会等 active_ 归零，因此租约存在时 root 必然有效
        unsafe { self.root_.as_ref() }
            .active_
            .fetch_sub(1, Ordering::AcqRel);
    }
}

/// 一整棵 Scope 树的根。
///
/// 字段全部私有；调用方只能通过 [`RootScope::try_init_in`] / [`RootScope::get`] /
/// [`RootScope::close`] 等入口使用它。root 发布后不再移动。
pub struct RootScope {
    /// 发布槽位的共享形式；最终清理时写回空指针。
    slot_: &'static AtomicPtr<RootScope>,

    /// root 状态位：`closing` / `finalize_elected` / `finalizing` 合并成一个原子字。
    ///
    /// 三个位都只在 root 级收尾路径上访问；合并之后不再需要三份独立的 `AtomicBool`，
    /// 也避免了“先读一个再读另一个”的多字快照问题。
    pub(super) flags_: RootFlags,

    /// 仍然存活（已创建、尚未登记退出）的域数量。
    pub(super) live_scopes_: AtomicUsize,

    /// 正在执行 `try_flush_` 的线程数；最终清理会等它归零。
    ///
    /// 计数覆盖“从进入 `try_flush_` 到完全退出该函数”的窗口，用来保证最后一个域登记
    /// 退出之后，不会有线程还站在实体内存上。
    pub(super) flush_inflight_: AtomicUsize,

    /// 当前持有 root 使用租约的线程数。
    ///
    /// `Scope::drop` / `Retain::drop` / `RootScope::close` 在触碰实体或 root 之前先取租约；
    /// 最终清理会等它归零，从而不会在别的线程还拿着实体 / 弱池指针时释放内存。
    pub(super) active_: AtomicUsize,

    /// 域实体的强块池 + 实体自己的身份弱槽位池。每个实体是一个
    /// `StrongChunk<ScopeEntity<A>, ()>`，弱槽位与强块都由这个池持有。
    ///
    /// 实体不需要额外的登记结构：本池的弱槽位链就是实体注册表，root 最终清理时
    /// 直接用与普通域相同的 `flush_data_` 遍历它。
    ///
    /// 分配 / 冻结由自旋锁串行化；同一个锁也用于把“创建新域”与“root finalize”串行化。
    entity_pool_: SpinningMutexOwned<ScopeInner>,

    /// 类型级 vtable 表：键是强块头部保存的合成 `ConstTypeId`。
    ///
    /// B+ 树的数据槽在条目存续期间地址稳定，但查询一律复制 [`AnyTypeVTable`] 后立刻
    /// 释放锁，因此强块只保存 id，不保存树内地址。
    metas_: SpinningMutexOwned<MetasBpTree>,

    /// 类型擦除后的分配器；root 自身的存储 / 实体池都由它分配。
    alloc_: &'static (dyn Allocator + Send + Sync),

    /// 单态化后的存储释放函数：把尾部的具体分配器移出，释放整块存储，再析构分配器。
    dealloc_storage_: unsafe fn(NonNull<RootScope>),
}

// SAFETY: `entity_pool_` / `metas_` 各自带自旋锁；`alloc_` 要求 `Send + Sync`；
// 原子字段与指针字段都只在显式同步下访问；root 发布后不再移动。
unsafe impl Send for RootScope {}
// SAFETY: 见上。
unsafe impl Sync for RootScope {}

impl RootScope {
    /// 在调用方提供的静态发布槽位上初始化一个 [`RootScope`]。
    ///
    /// - 槽位当前必须为空；并发调用只有一个能成功；
    /// - 成功时返回已发布的 root，后到的调用会以 `Err` 返回先到者发布的同一个 root；
    /// - root 的存储由 `allocator` 分配，最终清理时也由同一个分配器释放。
    ///
    /// # Errors
    ///
    /// 槽位已经被（或曾经被）初始化时返回 `Err` 指向已有 root。
    ///
    /// # Panics
    ///
    /// 分配 root 存储或类型表失败时 panic：root 是树的根，没有可恢复的调用方。
    ///
    /// # Examples
    ///
    /// ```
    /// use core::{ptr, sync::atomic::AtomicPtr};
    /// use std::alloc::Global;
    /// use sw_scope::RootScope;
    ///
    /// let slot: &'static mut AtomicPtr<RootScope> =
    ///     Box::leak(Box::new(AtomicPtr::new(ptr::null_mut())));
    /// let root = match RootScope::try_init_in(slot, 4096, Global) {
    ///     Ok(root) | Err(root) => root,
    /// };
    /// assert!(!root.is_closing());
    /// ```
    pub fn try_init_in<A>(
        slot: &'static mut AtomicPtr<RootScope>,
        page_size: usize,
        allocator: A,
    ) -> Result<&'static RootScope, &'static RootScope>
    where
        A: Allocator + Send + Sync + 'static,
    {
        let slot: &'static AtomicPtr<RootScope> = &*slot;
        // 哨兵取发布槽位自身的地址：非空，且不可能等于任何真实 root 的分配地址，
        // 因此 get() 可以据此区分“尚未初始化”“正在初始化”“已发布”三种状态。
        let sentinel = Self::sentinel_of_(slot);
        loop {
            let current = slot.load(Ordering::SeqCst);
            if ptr::eq(current, sentinel) {
                // 另一个线程正在初始化；等它发布或失败回滚。
                core::hint::spin_loop();
                continue;
            }
            if !current.is_null() {
                // SAFETY: 槽位里是已发布的 root；root 只在 finalize_ 里被清空并释放，
                // 而 finalize_ 之后槽位会变成 null，不会留下悬垂指针。
                return Result::Err(unsafe { &*current });
            }
            match slot.compare_exchange(
                ptr::null_mut(),
                sentinel,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Result::Ok(_) => break,
                Result::Err(_) => continue,
            }
        }
        match Self::alloc_storage_(slot, allocator, page_size) {
            Result::Ok(root) => {
                slot.store(root.as_ptr(), Ordering::Release);
                // SAFETY: root 刚分配、独占，且从此不再移动
                Result::Ok(unsafe { &*root.as_ptr() })
            }
            Result::Err(_) => {
                // 初始化失败：把哨兵换回空，避免其他线程永久自旋。
                slot.store(ptr::null_mut(), Ordering::Release);
                panic!("分配 RootScope 失败");
            }
        }
    }

    /// 读取发布槽位上已经初始化的 root；尚未初始化或正在初始化时返回 [`None`]。
    ///
    /// 正在初始化的线程会自旋等待发布完成。
    ///
    /// # 生命周期
    ///
    /// 返回的引用会一直有效到 root 的最终清理为止；最终清理只能由
    /// [`RootScope::close`]（unsafe）触发。因此并发调用 `close` 时，调用方必须保证没有
    /// 其它线程正在使用 `get` 的返回值——这份责任由 `close` 的 unsafe 契约承担。
    pub fn get(slot: &'static AtomicPtr<RootScope>) -> Option<&'static RootScope> {
        loop {
            let ptr = slot.load(Ordering::Acquire);
            if ptr.is_null() {
                return Option::None;
            }
            if ptr::eq(ptr, Self::sentinel_of_(slot)) {
                core::hint::spin_loop();
                continue;
            }
            // SAFETY: root 由 try_init_in 发布，在 finalize_ 清空槽位前一直有效
            return Option::Some(unsafe { &*ptr });
        }
    }

    /// 发出 root close 信号。
    ///
    /// 它只把 `closing_` 置真，并在实体池锁内与“创建新 Scope”串行化；所有已经存在的
    /// 域会拒绝新的放入，但不会立刻被析构。真正的最终清理由最后一个完成清空的域所在
    /// 线程负责；如果调用本函数时已经没有存活域，则由当前线程完成清理。
    ///
    /// # Safety
    ///
    /// 调用之后对由该 root 创建的 `Scope` / `Retain` / `Owning` / `Sharing` 只允许
    /// **析构**，不得再读取、升级或放入：
    ///
    /// - 这些句柄仍然是触发“域完成清空 / root 最终清理”的必要条件，必须继续 drop；
    /// - 最终清理会把 root 实体池（包括每个域的 `ScopeInner` 与弱池）以及 root 存储
    ///   归还给分配器；
    /// - `Retain` 没有 `'root` 生命周期，编译器无法阻止这些句柄跨越 close；
    /// - 若在最终清理之后再次解引用任何句柄，属于未定义行为；
    /// - 调用 `close` 期间不得有其它线程通过 [`RootScope::get`] 或任何句柄访问这棵树，
    ///   也不允许并发调用 `close`（本函数不是线程安全的“多方关闭”原语）。
    ///
    /// 允许在 `close` 之后继续使用**已经不存在**的句柄以外的普通值（例如发布槽位本身），
    /// 但槽位会被清空，之后再调用 [`RootScope::get`] 只会得到 `None`。
    ///
    /// # Examples
    ///
    /// ```
    /// use core::{ptr, sync::atomic::AtomicPtr};
    /// use std::alloc::Global;
    /// use sw_scope::{RootScope, Scope};
    ///
    /// let slot: &'static mut AtomicPtr<RootScope> =
    ///     Box::leak(Box::new(AtomicPtr::new(ptr::null_mut())));
    /// let root = match RootScope::try_init_in(slot, 4096, Global) {
    ///     Ok(root) | Err(root) => root,
    /// };
    /// let slot: &'static AtomicPtr<RootScope> = root.slot();
    /// let scope = Scope::new_in(root).expect("新建域应当成功");
    /// drop(scope);
    /// // SAFETY: 域已经析构，之后不再访问该 root 创建的句柄。
    /// unsafe { RootScope::close(slot) };
    /// assert!(RootScope::get(slot).is_none());
    /// ```
    pub unsafe fn close(slot: &'static AtomicPtr<RootScope>) {
        let Option::Some(root) = Self::get(slot) else {
            return;
        };
        // 先取使用租约：保证 close 检查期间 root 存储不会被另一个线程释放。
        let Option::Some(lease) = RootLease::enter_(NonNull::from(root)) else {
            return;
        };
        // 与“创建新 Scope”串行：创建路径在实体池锁内检查 closing_，因此这里必须在同一
        // 把锁内置真，否则并发创建可能越过 close 检查。
        {
            let mut session = root.entity_pool_.lock_session();
            let _guard = match session.lock().wait() {
                Result::Ok(guard) => guard,
                Result::Err(_) => unreachable!("root entity_pool 自旋锁不应失败"),
            };
            root.set_closing_();
        }
        // 没有存活域时立刻参与收尾选举。其它租约（并发 close、正在退出的最后一个域线程）
        // 由 `finalize_` 的 `active_ == 0` 等待覆盖；它们看到 `finalize_elected_` 后都会
        // 自行退出，不会与本线程互相等待。
        loop {
            if root.is_finalize_elected_() {
                return;
            }
            if root.live_scopes_.load(Ordering::SeqCst) != 0 {
                return;
            }
            if root.flush_inflight_.load(Ordering::SeqCst) != 0 {
                core::hint::spin_loop();
                continue;
            }
            if root.try_elect_root_finalize_() {
                drop(lease);
                // SAFETY: live_scopes_ == 0；其它租约持有者会在观察到 elected 后退出，
                // finalize_ 会等 active_ 归零再释放内存。
                unsafe { Self::finalize_(NonNull::from(root)) };
            }
            return;
        }
    }

    /// 读取合并后的 root 状态字（测试与内部判断共用）。
    #[inline]
    fn flags_raw_(&self) -> u32 {
        self.flags_.value()
    }

    /// root 是否已经收到 close 信号。
    #[inline]
    pub fn is_closing(&self) -> bool {
        self.flags_raw_() & FLAG_CLOSING != 0
    }

    /// 本 root 的发布槽位。
    ///
    /// 调用方通常已经把槽位保存在自己的静态变量里；本方法便于在只拿到 `&RootScope` 时
    /// 继续使用 [`RootScope::get`] / [`RootScope::close`]。
    #[inline]
    pub fn slot(&self) -> &'static AtomicPtr<RootScope> {
        self.slot_
    }

    #[inline]
    pub(super) fn is_closing_(&self) -> bool {
        self.is_closing()
    }

    /// 最终清理权是否已经被某个线程赢走。
    #[inline]
    pub(super) fn is_finalize_elected_(&self) -> bool {
        self.flags_raw_() & FLAG_FINALIZE_ELECTED != 0
    }

    /// 最终清理是否已经开始（新租约一律失败）。
    #[inline]
    pub(super) fn is_finalizing_(&self) -> bool {
        self.flags_raw_() & FLAG_FINALIZING != 0
    }

    /// 置 close 位。
    #[inline]
    fn set_closing_(&self) {
        self.flags_
            .as_ref()
            .fetch_or(FLAG_CLOSING, Ordering::SeqCst);
    }

    /// 置 finalizing 位；之后不再接受新的租约。
    #[inline]
    fn set_finalizing_(&self) {
        self.flags_
            .as_ref()
            .fetch_or(FLAG_FINALIZING, Ordering::SeqCst);
    }

    /// 尝试赢得最终清理权。
    ///
    /// 已处于 `FINALIZE_ELECTED` 或 `FINALIZING` 时返回 `false`；成功置位返回 `true`。
    /// 用一个 CAS 完成“检查 + 置位”，因此 `close` 与最后一个域线程不会同时认为自己
    /// 赢得了清理权。
    pub(super) fn try_elect_root_finalize_(&self) -> bool {
        matches!(
            self.flags_.try_spin_compare_exchange_weak(
                |raw| raw & FLAG_FINALIZE_TERMINAL == 0,
                |raw| raw | FLAG_FINALIZE_ELECTED,
            ),
            CmpxchResult::Succ(_)
        )
    }

    /// 在 root 的实体池上执行一次操作。
    ///
    /// 这把锁同时承担“创建域 vs root 最终清理”的互斥：创建路径在锁内检查 `closing_`，
    /// 最终清理在锁内回收实体池。
    pub(crate) fn with_entity_pool_<R>(&self, op: impl FnOnce(&mut ScopeInner) -> R) -> R {
        let mut session = self.entity_pool_.lock_session();
        let mut guard = match session.lock().wait() {
            Result::Ok(guard) => guard,
            Result::Err(_) => unreachable!("root entity_pool 自旋锁不应失败"),
        };
        op(&mut guard)
    }

    /// 创建并发布一个新的域实体。
    ///
    /// 调用方必须持有 `&'static self`；实体池锁会与 root close / 最终清理串行化。
    pub(crate) fn try_new_scope_<A, C>(
        &'static self,
        alloc: A,
    ) -> Result<crate::Scope, crate::ScopeError>
    where
        A: Allocator + Send + Sync + 'static,
        C: crate::TrScopeInnerConfig,
    {
        let page_size = <C::PageSize as crate::page_size::TrPageSizeSelect>::PAGE_SIZE;
        let mut session = self.entity_pool_.lock_session();
        let mut pool = match session.lock().wait() {
            Result::Ok(guard) => guard,
            Result::Err(_) => unreachable!("root entity_pool 自旋锁不应失败"),
        };
        if self.is_closing_() {
            return Result::Err(crate::ScopeError::ClosedRoot);
        }
        let chunk = ScopeEntity::<A>::allocate_entity_in_(self, &mut pool, alloc, page_size)
            .map_err(|_| crate::ScopeError::MallocFailed)?;
        // 实体已经在实体池的弱槽位链上登记；这里只增加“尚未退出”的域计数。
        self.live_scopes_.fetch_add(1, Ordering::SeqCst);
        Result::Ok(crate::Scope::from_chunk_(chunk))
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
    /// `Drop` / 构造；因此持锁调用它是安全的。
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

    /// 进入一次 `try_flush_`；与 [`RootScope::finalize_`] 的等待配对。
    pub(crate) fn begin_flush_(&self) {
        self.flush_inflight_.fetch_add(1, Ordering::SeqCst);
    }

    /// 退出一次 `try_flush_`。
    pub(crate) fn end_flush_(&self) {
        self.flush_inflight_.fetch_sub(1, Ordering::SeqCst);
    }

    /// 最终清理：释放整棵树。
    ///
    /// 调用方必须已经通过 `flags_` 里的最终清理选举，并且没有其它 root 租约持有者
    /// （`active_ == 0`）。这里会释放 root 存储本身，因此返回后不得再解引用 `root`。
    ///
    /// # Safety
    ///
    /// 同上；此外，调用时不得有任何 `Scope` / `Retain` 仍指向本树（这由每个域退出时
    /// 把 `live_scopes_` 减到 0，以及 `finalize_elected_` 的唯一选举保证）。
    pub(crate) unsafe fn finalize_(root: NonNull<RootScope>) {
        // SAFETY: 调用方保证 root 仍然有效且独占
        let root_ref = unsafe { root.as_ref() };
        // 置 finalizing 之后，新的租约申请一律失败；再等所有旧租约 / flush 退出。
        root_ref.set_finalizing_();
        while root_ref.active_.load(Ordering::SeqCst) != 0
            || root_ref.flush_inflight_.load(Ordering::SeqCst) != 0
        {
            core::hint::spin_loop();
        }
        // 1) 把 root 实体池当成一个普通域来清盘：实体 chunk 就是它的“用户数据”，
        //    实体身份槽位就是普通的弱槽位。`flush_data_` 会枚举弱池链上的每个槽位、
        //    通过实体 vtable 析构 `ScopeEntity<A>`（锁内核 + 每域分配器），并在所有
        //    实体死亡后整链回收实体强池。
        //    因此 root 不再需要单独的实体链表 / 实体身份指针。
        {
            let mut session = root_ref.entity_pool_.lock_session();
            let mut pool = match session.lock().wait() {
                Result::Ok(guard) => guard,
                Result::Err(_) => unreachable!("root entity_pool 自旋锁不应失败"),
            };
            // SAFETY: live_scopes_ == 0，所有域都已退出；closing 已置位，实体池不会再
            // 接受新实体。实体池没有自己的 ScopeLock，但最终清理已独占它。
            unsafe { pool.flush_data_() };
            // SAFETY: 实体都已析构、强池已回收；剩余弱池只承载 Destroyed 的身份槽位
            unsafe { pool.free_all_weak_pools_() };
        }
        // 2) 释放 per-root 类型表：必须在所有用户数据（含实体）析构完成之后。
        // SAFETY: 没有任何线程还会查表；drop_in_place 只析构 B+ 树本身
        unsafe { ptr::drop_in_place(root_ref.metas_.as_mut_ptr()) };
        // 3) 清空发布槽位，之后 get() 返回 None。
        let slot = root_ref.slot_;
        slot.store(ptr::null_mut(), Ordering::Release);
        // 4) 释放 root 存储（包括尾部那个具体分配器）。
        let dealloc_storage = root_ref.dealloc_storage_;
        // SAFETY: 已清空槽位，且没有其它引用；dealloc_storage 会先移出分配器再释放整块存储
        unsafe { dealloc_storage(root) };
    }

    /// 当前线程持有且只持有**一个** root 租约时，尝试赢得最终清理权。
    ///
    /// 返回 `true` 表示调用方赢得了 `flags_` 的 `FINALIZE_ELECTED` 位；调用方必须先归还自己的租约，
    /// 再调用 [`RootScope::finalize_`]。返回 `false` 表示条件不满足或别人已经赢得选举。
    ///
    /// 这是“最后一个域线程先观察到 `closing == false`、随后 close 才置位”的补丁点：
    /// 域线程在退出前会再检查一次；若此时只剩自己持有租约，就由它完成清理。
    pub(crate) fn try_elect_finalize_(&self) -> bool {
        if self.is_finalize_elected_() || self.is_finalizing_() {
            return false;
        }
        if !self.is_closing_() || self.live_scopes_.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if self.flush_inflight_.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if self.active_.load(Ordering::SeqCst) != 1 {
            return false;
        }
        self.try_elect_root_finalize_()
    }

    /// 发布槽位的初始化哨兵。
    fn sentinel_of_(slot: &'static AtomicPtr<RootScope>) -> *mut RootScope {
        slot as *const AtomicPtr<RootScope> as *const u8 as *const RootScope as *mut RootScope
    }

    /// 分配并初始化 root 存储。
    fn alloc_storage_<A>(
        slot: &'static AtomicPtr<RootScope>,
        allocator: A,
        page_size: usize,
    ) -> Result<NonNull<RootScope>, AllocError>
    where
        A: Allocator + Send + Sync + 'static,
    {
        let layout = Layout::new::<RootScopeStorage<A>>();
        let mem = allocator.allocate(layout)?;
        let storage = mem.as_ptr() as *mut RootScopeStorage<A>;
        let metas = match MetasBpTree::new() {
            Result::Ok(tree) => tree,
            Result::Err(_) => {
                // SAFETY: mem 是刚由 allocator 按 layout 分配、尚未使用的内存
                unsafe { allocator.deallocate(mem.cast::<u8>(), layout) };
                return Result::Err(AllocError);
            }
        };
        // SAFETY: storage 是刚分配、独占、足够大的内存；下面每个字段只写一次。
        unsafe {
            ptr::addr_of_mut!((*storage).alloc).write(allocator);
            let alloc_ref: &'static (dyn Allocator + Send + Sync) =
                &*ptr::addr_of!((*storage).alloc);
            let root = ptr::addr_of_mut!((*storage).root);
            let root_nn = NonNull::new_unchecked(root);
            ptr::addr_of_mut!((*root).slot_).write(slot);
            ptr::addr_of_mut!((*root).flags_).write(AtomicFlags::new(AtomicU32::new(0)));
            ptr::addr_of_mut!((*root).live_scopes_).write(AtomicUsize::new(0));
            ptr::addr_of_mut!((*root).flush_inflight_).write(AtomicUsize::new(0));
            ptr::addr_of_mut!((*root).active_).write(AtomicUsize::new(0));
            ptr::addr_of_mut!((*root).alloc_).write(alloc_ref);
            ptr::addr_of_mut!((*root).dealloc_storage_)
                .write(dealloc_root_storage_::<A> as unsafe fn(NonNull<RootScope>));
            // root 自己的实体池也要能沿 scope_ -> root_ 找回 root。
            let mut pool = ScopeInner::empty_(NonNull::from(alloc_ref), page_size);
            pool.set_root_(root_nn);
            ptr::addr_of_mut!((*root).entity_pool_).write(SpinningMutexOwned::new_owned(pool));
            ptr::addr_of_mut!((*root).metas_).write(SpinningMutexOwned::new_owned(metas));
            Result::Ok(root_nn)
        }
    }
}

/// 释放 root 存储：先把尾部的具体分配器移出，再按原布局归还内存，最后析构分配器。
///
/// # Safety
///
/// `root` 必须来自 `RootScope::alloc_storage_::<A>` 的返回，且已经完成全部字段级清理。
unsafe fn dealloc_root_storage_<A>(root: NonNull<RootScope>)
where
    A: Allocator + 'static,
{
    let storage = root.as_ptr() as *mut RootScopeStorage<A>;
    let layout = Layout::new::<RootScopeStorage<A>>();
    // SAFETY: storage 由同一布局分配；alloc 字段只读取一次并移出
    let alloc = unsafe { ptr::read(ptr::addr_of!((*storage).alloc)) };
    // SAFETY: root 与 layout 来自同一次 allocate
    unsafe { alloc.deallocate(root.cast::<u8>(), layout) };
    drop(alloc);
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 旧实现（保留作对照，不要据此恢复布局假设）
//
// pub(crate) type RootScopeRef<const CELL_SIZE: usize> = &'static ScopeInner<CELL_SIZE>;
// ...
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

#[cfg(test)]
mod tests_ {
    use super::*;
    use crate::{
        Scope,
        scope_inner_::{DEFAULT_PAGE_SIZE, DefaultConfig},
        test_support_::test_root_,
    };
    use core::sync::atomic::AtomicUsize;
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

        let root = test_root_();
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

        static VT_DROPS: AtomicUsize = AtomicUsize::new(0);

        struct VtDropProbe;

        impl Drop for VtDropProbe {
            fn drop(&mut self) {
                VT_DROPS.fetch_add(1, Ordering::AcqRel);
            }
        }

        VT_DROPS.store(0, Ordering::Release);
        let root = test_root_();
        let vtable = root.ensure_chunk_meta_::<VtDropProbe, ()>();

        let mut value = ManuallyDrop::new(VtDropProbe);
        let data = (&mut *value as *mut VtDropProbe).cast::<()>();
        // SAFETY: data 指向仍存活、尚未析构的 VtDropProbe；metadata 为 Sized 的空值
        unsafe { vtable.run_drop_(data, core::ptr::null()) };

        assert_eq!(VT_DROPS.load(Ordering::Acquire), 1, "数据必须恰好析构一次");
    }

    /// 两个独立 root 的类型表互不串表，且各自都能完成类型擦除析构。
    /// Two independent roots keep separate type tables and each performs erased destruction.
    ///
    /// - 手段：用两个独立发布槽位各初始化一个 root；在两个 root 下各放一个同类型的
    ///   `DropProbe`，逐个释放句柄，最后分别 close 并检查槽位被清空。
    ///   / How: initialise two roots on two distinct publication slots, put one `DropProbe` of
    ///   the same type under each root, drop the handles one by one, close both roots and check
    ///   that both slots become null.
    /// - 判断：两个 root 互不相等；两个对象各自析构一次；两个槽位最终都为空。
    ///   / What: the roots differ, each object is dropped exactly once, and both slots end up
    ///   null.
    #[compio::test]
    async fn independent_roots_do_not_share_type_tables() {
        static DROPS: AtomicUsize = AtomicUsize::new(0);

        struct DropProbe;

        impl Drop for DropProbe {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::AcqRel);
            }
        }

        DROPS.store(0, Ordering::Release);
        let root_a = test_root_();
        let root_b = test_root_();
        let slot_a = root_a.slot();
        let slot_b = root_b.slot();
        assert!(!core::ptr::eq(root_a, root_b), "两个 root 必须是不同实例");

        let scope_a = Scope::new_in(root_a).expect("root a 建域");
        let scope_b = Scope::new_in(root_b).expect("root b 建域");
        let retain_a = scope_a.put_async(DropProbe).await.expect("root a 放入");
        let retain_b = scope_b.put_async(DropProbe).await.expect("root b 放入");
        drop(retain_a);
        assert_eq!(DROPS.load(Ordering::Acquire), 1);
        drop(retain_b);
        assert_eq!(DROPS.load(Ordering::Acquire), 2);

        drop(scope_a);
        drop(scope_b);
        // SAFETY: 两个域都已析构，之后不再访问它们创建的句柄
        unsafe {
            RootScope::close(slot_a);
            RootScope::close(slot_b);
        }
        assert!(RootScope::get(slot_a).is_none());
        assert!(RootScope::get(slot_b).is_none());
    }

    /// root close 之后所有域都拒绝放入，且最后一个域退出后 root 被最终清理。
    /// After root close every scope rejects puts, and the root is finalised once the last
    /// scope exits.
    ///
    /// - 手段：初始化一个 root，建立一个域并放入一个值；先调用 `RootScope::close`，断言
    ///   再次放入返回 `ClosedRoot`；随后释放 `Retain` 与全部 `Scope` 句柄。
    ///   / How: initialise a root, create a scope with one value, call `RootScope::close`,
    ///   check that another put fails with `ClosedRoot`, then drop the `Retain` and all
    ///   `Scope` handles.
    /// - 判断：close 后放入失败；最后一个域退出后发布槽位变成空指针，说明 root 已经完成
    ///   最终清理（实体池、类型表与 root 存储都已释放）。
    ///   / What: the put fails after close, and the publication slot becomes null once the last
    ///   scope exits, proving the root has been finalised.
    #[compio::test]
    async fn root_close_stops_puts_and_finalises_after_last_scope() {
        let root = test_root_();
        let slot = root.slot();
        let scope = Scope::new_in(root).expect("建域应当成功");
        let retain = scope.put_async(7u32).await.expect("放入应当成功");
        let sibling = scope.clone();

        // SAFETY: 测试随后会先释放全部句柄，再使用槽位本身；不访问 root 创建的句柄
        unsafe { RootScope::close(slot) };
        assert!(root.is_closing(), "close 必须置位 closing 状态");
        assert!(
            RootScope::get(slot).is_some(),
            "还有存活域时 root 不能被释放"
        );
        assert!(
            matches!(
                scope.put_async(9u32).await,
                Result::Err(crate::ScopeError::ClosedRoot)
            ),
            "root close 后不得再分配"
        );

        drop(retain);
        drop(sibling);
        drop(scope);
        assert!(
            RootScope::get(slot).is_none(),
            "最后一个域退出后 root 必须被最终清理并清空槽位"
        );
    }

    /// root 状态位的置位与最终清理选举互斥。
    /// Root state bits are set correctly and the finalisation election is exclusive.
    ///
    /// - 手段：在一个测试 root 上置 closing 位；用 8 个线程同时调用
    ///   `try_elect_root_finalize_`，统计成功次数；最后置 finalizing 位再尝试选举。
    ///   / How: set the closing bit on a test root, let eight threads call
    ///   `try_elect_root_finalize_` concurrently and count successes, then set the finalizing
    ///   bit and try to elect again.
    /// - 判断：closing 置位后可见；选举恰好成功一次；finalizing 置位后选举失败，且
    ///   `is_finalize_elected_` / `is_finalizing_` 都读到对应位。
    ///   / What: closing is visible, exactly one election succeeds, and after the finalizing bit
    ///   is set a new election fails while both flag bits read back correctly.
    #[test]
    fn root_flags_and_finalize_election_are_exclusive() {
        use std::sync::{Arc, Barrier};

        let root = test_root_();
        assert!(!root.is_closing());
        assert!(!root.is_finalize_elected_());
        assert!(!root.is_finalizing_());

        root.set_closing_();
        assert!(root.is_closing(), "closing 位必须立即可见");

        let winners = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = std::vec::Vec::new();
        for _ in 0..8 {
            let winners = Arc::clone(&winners);
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                if root.try_elect_root_finalize_() {
                    winners.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for thread in threads {
            thread.join().expect("选举线程不应 panic");
        }
        assert_eq!(
            winners.load(Ordering::Acquire),
            1,
            "最终清理选举必须恰好成功一次"
        );
        assert!(root.is_finalize_elected_());
        assert!(!root.try_elect_root_finalize_(), "已选举后不得再次成功");

        root.set_finalizing_();
        assert!(root.is_finalizing_());
        assert!(!root.try_elect_root_finalize_(), "finalizing 后不得再选举");
        // 本测试不 finalize root：只验证状态位与选举语义，root 由测试进程自然结束。
    }

    /// `RootScope::get` 对未初始化的槽位返回 `None`，初始化后可取回同一个实例。
    /// `RootScope::get` returns `None` for an empty slot and the same instance afterwards.
    ///
    /// - 手段：先用空槽位调用 `get`，再初始化 root 并再次调用。
    ///   / How: call `get` on an empty slot first, then initialise the root and call `get` again.
    /// - 判断：第一次为 `None`，第二次与 `try_init_in` 返回的引用指向同一地址。
    ///   / What: the first call yields `None`, the second points at the same address as the
    ///   reference returned by `try_init_in`.
    #[test]
    fn root_get_reflects_publication_state() {
        let raw: *mut AtomicPtr<RootScope> =
            alloc::boxed::Box::into_raw(alloc::boxed::Box::new(AtomicPtr::new(ptr::null_mut())));
        // SAFETY: raw 指向测试独占的槽位，尚未被任何引用别名
        assert!(
            RootScope::get(unsafe { &*raw }).is_none(),
            "空槽位必须返回 None"
        );
        // SAFETY: raw 指向测试独占的槽位；&mut 被 try_init_in 消费后不再存在
        let slot: &'static mut AtomicPtr<RootScope> = unsafe { &mut *raw };
        let root = match RootScope::try_init_in(slot, DEFAULT_PAGE_SIZE, Global) {
            Result::Ok(root) | Result::Err(root) => root,
        };
        let got = RootScope::get(root.slot()).expect("发布后应当能取回 root");
        assert!(core::ptr::eq(root, got), "get 必须返回同一个实例");
        // 不 close：本测试只验证发布状态，root 由测试进程自然结束。
    }

    /// 用一个默认配置的域验证 root 实体池能创建普通 Scope。
    /// Use a default-config scope to check that the root entity pool can create scopes.
    ///
    /// - 手段：初始化 root 后连续创建 64 个域。
    ///   / How: initialise a root and create 64 scopes.
    /// - 判断：全部创建成功且互不相等。
    ///   / What: all creations succeed and the handles are pairwise distinct.
    #[test]
    fn root_entity_pool_creates_many_scopes() {
        let root = test_root_();
        let scopes: std::vec::Vec<Scope> = (0..64)
            .map(|_| Scope::new_in_with_cfg::<DefaultConfig<Global>>(root).expect("建域"))
            .collect();
        assert!(scopes[0] != scopes[63], "实体地址必须互不相同");
    }

    /// 测试 `Scope::new_in` 创建的实体最终经实体 vtable 析构每域分配器。
    /// Test that a scope created by `Scope::new_in` is finally dropped through the entity vtable.
    ///
    /// - 手段：用一个会把析构计数加一的分配器创建域，释放句柄后关闭 root。
    ///   / How: create a scope with an allocator that counts drops, drop the handle, then close
    ///   the root.
    /// - 判断：分配器析构计数恰好为一，说明实体析构走了 `ScopeEntity<A>` 的 vtable。
    ///   / What: the allocator's drop count is exactly one, showing the entity was dropped via
    ///   the `ScopeEntity<A>` vtable.
    #[compio::test]
    async fn entity_vtable_drops_scope_allocator() {
        static ALLOC_DROPS: AtomicUsize = AtomicUsize::new(0);

        #[derive(Default)]
        struct DropCountingAlloc;

        impl Drop for DropCountingAlloc {
            fn drop(&mut self) {
                ALLOC_DROPS.fetch_add(1, Ordering::AcqRel);
            }
        }

        // SAFETY: 只把分配 / 释放原样转交给 Global
        unsafe impl Allocator for DropCountingAlloc {
            fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                Global.allocate(layout)
            }

            unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
                // SAFETY: 指针与布局来自上面的 allocate
                unsafe { Global.deallocate(ptr, layout) }
            }
        }

        ALLOC_DROPS.store(0, Ordering::Release);
        let root = test_root_();
        let slot = root.slot();
        let scope = Scope::new_in_with_alloc::<DropCountingAlloc, DefaultConfig<Global>>(root)
            .expect("建域");
        drop(scope);
        // SAFETY: 域已释放，之后不再访问它创建的句柄
        unsafe { RootScope::close(slot) };
        assert_eq!(
            ALLOC_DROPS.load(Ordering::Acquire),
            1,
            "实体 vtable 必须恰好析构一次每域分配器"
        );
    }

    /// root close 后若仍有 `Retain` 存活，最终清理由最后一个 `Retain` 的 drop 线程完成。
    /// If a `Retain` outlives root close, the last `Retain` drop thread performs finalisation.
    ///
    /// - 手段：在一个域里放入一个值，先释放 `Scope` 句柄（数据仍由 `Retain` 保活），再
    ///   调用 `RootScope::close`，最后释放 `Retain`。
    ///   / How: put one value, drop the `Scope` handle first (the `Retain` keeps the data
    ///   alive), call `RootScope::close`, then drop the `Retain`.
    /// - 判断：close 时槽位仍非空；最后一个 `Retain` 释放后槽位变成空指针，说明 root 由
    ///   该线程完成最终清理。
    ///   / What: the slot is still non-null at close time, and becomes null after the last
    ///   `Retain` is dropped, showing that thread performed the root finalisation.
    #[compio::test]
    async fn root_close_waits_for_last_retain_then_finalises() {
        let root = test_root_();
        let slot = root.slot();
        let retain = {
            let scope = Scope::new_in(root).expect("建域应当成功");
            scope.put_async(11u32).await.expect("放入应当成功")
            // scope 在这里析构：句柄归零，但数据仍由 retain 保活
        };

        // SAFETY: 测试随后会先释放 Retain，再使用槽位本身；不访问 root 创建的句柄
        unsafe { RootScope::close(slot) };
        assert!(
            RootScope::get(slot).is_some(),
            "仍有 Retain 存活时 root 不能被释放"
        );
        drop(retain);
        assert!(
            RootScope::get(slot).is_none(),
            "最后一个 Retain 释放后 root 必须被最终清理"
        );
    }

    /// root close 与“最后一个域析构”并发时仍能完成最终清理。
    /// Concurrent root close and last-scope drop still finish the finalisation exactly once.
    ///
    /// - 手段：反复多轮创建两个域，先析构一个，再用屏障让另一个线程析构最后一个域、主线程
    ///   同时调用 `RootScope::close`。
    ///   / How: in each round create two scopes, drop one, then use a barrier so a worker
    ///   thread drops the last scope while the main thread calls `RootScope::close`.
    /// - 判断：每一轮 join 之后槽位都是空指针，说明两边的收尾选举没有漏掉清理、也没有
    ///   双重释放（后者会直接崩溃）。
    ///   / What: after every join the slot is null, showing the finalisation election neither
    ///   misses cleanup nor double-frees (the latter would crash).
    #[test]
    fn root_close_races_with_last_scope_drop() {
        use std::sync::{Arc, Barrier};

        for _ in 0..64 {
            let root = test_root_();
            let slot = root.slot();
            let first = Scope::new_in(root).expect("建域应当成功");
            let last = Scope::new_in(root).expect("建域应当成功");
            drop(first);

            let barrier = Arc::new(Barrier::new(2));
            let worker_barrier = Arc::clone(&barrier);
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                drop(last);
            });
            barrier.wait();
            // SAFETY: 另一个线程只做“析构最后一个域”，不再读取或升级任何数据；主线程
            // 在 join 之后才使用槽位本身。
            unsafe { RootScope::close(slot) };
            worker.join().expect("析构线程不应 panic");
            assert!(
                RootScope::get(slot).is_none(),
                "并发 close 与最后一个域析构必须完成最终清理"
            );
        }
    }

    /// root close 后不能再创建新域。
    /// No new scope can be created after root close.
    ///
    /// - 手段：建立一个域后 close root，再次调用 `Scope::new_in`，最后释放先前的域。
    ///   / How: create one scope, close the root, call `Scope::new_in` again, then drop the
    ///   original scope.
    /// - 判断：第二次创建返回 `ClosedRoot`；释放原域后 root 被清空。
    ///   / What: the second creation returns `ClosedRoot`, and the root slot is cleared once the
    ///   original scope is dropped.
    #[compio::test]
    async fn root_close_rejects_new_scopes() {
        let root = test_root_();
        let slot = root.slot();
        let scope = Scope::new_in(root).expect("建域应当成功");

        // SAFETY: 测试随后会先释放所有域，再使用槽位本身
        unsafe { RootScope::close(slot) };
        assert!(
            matches!(
                Scope::new_in(root),
                Result::Err(crate::ScopeError::ClosedRoot)
            ),
            "root close 后不得创建新域"
        );
        drop(scope);
        assert!(RootScope::get(slot).is_none());
    }
}
