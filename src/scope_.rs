//! [`Scope`]：整棵树里一个域的公开句柄。
//!
//! # 新设计（重构中）
//!
//! # 实验分支 feat/scope-lock
//!
//! - [`Scope`] 直接指向域实体的 `StrongChunk`；chunk 数据区首字段是同步自旋锁
//!   `SpinningMutexOwned<ScopeInner>`，因此 `ScopeInner` 内联在锁里，没有单独的头部或堆分配；
//! - 每域一份的池分配器 `A` 内联在实体尾部，`ScopeInner::allocator_` 指向它；
//! - [`Scope`] 实现 `Clone`：锁外原子句柄计数，最后一个句柄只做关闭标记；
//! - 显式 [`Scope::close`] 会在没有 `Owning` / `Sharing` 直接视图时清盘本域全部
//!   `Created` 对象，因此循环引用不依赖链表自身的 `clear`；
//! - 所有放入 / emplace 操作都是同步的：先 `lock().wait()` 拿自旋锁，再调用 `ScopeInner`。
//!
//! 实体由 `RootScope` 的分配器分配并登记到 root 的 `scopes_` 树；当前实验分支尚未实现
//! 实体回收与 `?Sized` 元信息登记。旧实现保留在文件末尾的块注释中。

use core::{
    alloc::{Allocator, Layout},
    mem::MaybeUninit,
    ptr::NonNull,
};

use crate::{
    abs_::{IntoEmplace, TrEmplace, TrPredrop, TrScope},
    index_::Retain,
    scope_inner_::{self, DEFAULT_PAGE_SIZE, RootScope, ScopeEntity, ScopeInner, ScopeLock},
    scope_str_::ScopeStr,
    strong_::StrongChunkBase,
};

pub use scope_inner_::TrScopeInnerConfig;

#[derive(Debug)]
pub enum ScopeError {
    /// This is rare but still possible
    MallocFailed,

    /// Error occurs during init
    MalformedInit,

    /// 目标 Scope（或其祖先）已被标记清盘，而 `strict-put-after-close` feature 要求报错。
    ///
    /// 未开启该 feature 时，关闭标记会被忽略，放入照常成功。
    ClosedScope,
}

/// 一个域的公开句柄：与 `Sharing` 类似，直接指向域实体的 `StrongChunk`。
///
/// 不再有单独的 `ScopeHeader`：chunk 的数据区就是 `ScopeEntity<A>`，其首字段是
/// `ScopeLock`（内含 `ScopeInner`），尾部是每域分配器。`Scope` 不需要知道 `A`，
/// 因为锁、句柄计数和分配器都可以从 chunk 的数据地址 / `ScopeInner` 取得。
pub struct Scope {
    /// 域实体的强块首址；root 的实体池保证它在进程内稳定。
    chunk_: NonNull<StrongChunkBase>,
}

impl Scope {
    /// 用给定配置的页大小初始化默认 root。
    ///
    /// 后到的并发调用会拿到先到者发布的 root；返回 `true` 表示 root 已可用。
    pub fn try_init_root<TyCfg>() -> bool
    where
        TyCfg: TrScopeInnerConfig,
    {
        let page_size = <TyCfg::PageSize as crate::page_size::TrPageSizeSelect>::PAGE_SIZE;
        // root 自己的实体池使用配置的分配器类型；已发布的 root 不会被替换。
        let root_alloc = <TyCfg::PoolAlloc as Default>::default();
        let _ = RootScope::try_init_default_root_scope_(page_size, root_alloc);
        true
    }

    /// 从默认的 RootScope 中创建一个子 scope。
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::new_with_cfg::<scope_inner_::DefaultConfig<alloc::alloc::Global>>()
    }

    /// 按给定配置创建域句柄：`C::PoolAlloc` 决定每域分配器类型，
    /// `C::PageSize` 决定开新池的页大小。
    pub fn new_with_cfg<C>() -> Self
    where
        C: TrScopeInnerConfig,
    {
        Self::new_with_alloc::<C::PoolAlloc, C>()
    }

    /// 按给定配置创建域句柄，池分配器使用 `A::default()`。
    pub fn new_with_alloc<A, C>() -> Self
    where
        A: Allocator + Default + Send + Sync + 'static,
        C: TrScopeInnerConfig,
    {
        Self::new_with_alloc_value::<A, C>(A::default())
    }

    /// 按给定配置与具体分配器值创建域句柄。
    ///
    /// `A` 是每域一份的池分配器，直接内联存放在实体尾部，不会单独堆分配。
    ///
    /// # Panics
    ///
    /// root 分配器无法提供实体内存时 panic；需要可恢复错误时请后续接入
    /// `try_new_with_alloc_value`。
    pub fn new_with_alloc_value<A, C>(alloc: A) -> Self
    where
        A: Allocator + Send + Sync + 'static,
        C: TrScopeInnerConfig,
    {
        let root = Self::default_root_();
        let page_size = <C::PageSize as crate::page_size::TrPageSizeSelect>::PAGE_SIZE;
        match ScopeEntity::<A>::allocate_entity_(root, alloc, page_size) {
            Result::Ok(chunk_) => Scope { chunk_ },
            Result::Err(_) => panic!("分配 ScopeEntity 失败"),
        }
    }

    /// 同步放入一个值。
    ///
    /// 失败只来自弱池 / 强池分配，或域已关闭。
    ///
    /// # Errors
    ///
    /// 域已关闭，或池分配失败时返回 [`ScopeError`]。
    pub fn put<T>(&self, data: T) -> Result<Retain<T>, ScopeError> {
        self.with_inner_(|inner| inner.put_value_(data))?
            .map_err(|_| ScopeError::MallocFailed)
    }

    /// 在同步自旋锁内访问本域状态。
    fn with_inner_<R>(&self, op: impl FnOnce(&mut ScopeInner) -> R) -> Result<R, ScopeError> {
        if self.inner_().is_closed_() {
            return Result::Err(ScopeError::ClosedScope);
        }
        let mut session = self.lock_().lock_session();
        let mut guard = match session.lock().wait() {
            Result::Ok(guard) => guard,
            // 同步自旋锁没有 poisoning 语义；把异常状态映射成关闭错误。
            Result::Err(_) => return Result::Err(ScopeError::ClosedScope),
        };
        Result::Ok(op(&mut guard))
    }

    /// 显式关闭本域：拒绝后续放入，并在没有 `Owning` / `Sharing` 直接视图时清盘。
    ///
    /// 清盘会强制析构本域所有**只由 `Retain` 持有**的 `Created` 对象，因此循环引用
    /// 不需要链表自己 `clear`。`Owning` / `Sharing` 指向的对象本轮跳过并保留，等视图
    /// 释放后再清盘。指向已清除对象的 `Retain` 会变成失效句柄（`try_owning` /
    /// `try_sharing` 返回 [`None`]），但不会悬空。
    ///
    /// 该方法消费一个 `Scope` 句柄；若还有其他克隆句柄，它们同样会看到关闭状态。
    pub fn close(self) {
        self.inner_().request_close_();
    }

    /// 取默认 root；并发初始化时后到者拿到先到者发布的 root。
    fn default_root_() -> &'static RootScope {
        match RootScope::try_init_default_root_scope_(DEFAULT_PAGE_SIZE, alloc::alloc::Global) {
            Result::Ok(root) | Result::Err(root) => root,
        }
    }

    /// 实体 chunk 首址。
    fn chunk_(&self) -> &StrongChunkBase {
        // SAFETY: 实体由 'static root 持有，且在句柄存活期间不会被释放
        unsafe { self.chunk_.as_ref() }
    }

    /// 实体里的锁；chunk 数据区首字段就是 `ScopeLock`。
    fn lock_(&self) -> &ScopeLock {
        // SAFETY: 数据区按 ScopeEntity<A> 布局初始化，lock_ 是首字段
        unsafe { &*self.chunk_().data_addr_().cast::<ScopeLock>() }
    }

    /// 锁里的域状态。
    fn inner_(&self) -> &ScopeInner {
        // SAFETY: lock 由实体持有；ScopeInner 内联在锁的 UnsafeCell 中，地址稳定
        unsafe { &*self.lock_().as_mut_ptr() }
    }
}

impl TrScope for Scope {
    type Err = ScopeError;

    unsafe fn emplace_with_predrop<M, P>(
        &self,
        layout: Layout,
        emplace: M,
        predrop: P,
    ) -> Result<Retain<M::Target>, Self::Err>
    where
        M: TrEmplace,
        P: TrPredrop<M::Target>,
    {
        scope_emplace_with_predrop_(self, layout, emplace, predrop)
    }

    fn put_with_factory_predrop<Ft, Fp, P, T>(
        &self,
        val_factory: Ft,
        predrop_factory: Fp,
    ) -> Result<Retain<T>, Self::Err>
    where
        Ft: FnOnce() -> T,
        Fp: FnOnce() -> P,
        P: TrPredrop<T>,
    {
        scope_put_with_factory_predrop_(self, val_factory, predrop_factory)
    }

    fn put_str_with_predrop<P>(&self, str: &str, predrop: P) -> Result<Retain<ScopeStr>, Self::Err>
    where
        P: TrPredrop<ScopeStr>,
    {
        scope_put_str_with_predrop_(self, str, predrop)
    }

    fn alloc_uninit_slice<P, T>(
        &self,
        length: usize,
        predrop: P,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err>
    where
        P: TrPredrop<[MaybeUninit<T>]>,
    {
        scope_alloc_uninit_slice_(self, length, predrop)
    }
}

impl core::cmp::PartialEq for Scope {
    fn eq(&self, other: &Self) -> bool {
        self.chunk_ == other.chunk_
    }
}

impl core::cmp::Eq for Scope {}

impl Clone for Scope {
    fn clone(&self) -> Self {
        self.inner_().incr_handle_();
        Scope {
            chunk_: self.chunk_,
        }
    }
}

// SAFETY: 实体由 root 持有，句柄只做原子计数；ScopeInner 的访问由实体锁串行化。
unsafe impl Send for Scope {}
// SAFETY: 见上。
unsafe impl Sync for Scope {}

impl Drop for Scope {
    /// 句柄析构：减引用计数；最后一个句柄只做关闭标记。
    ///
    /// 普通析构**不**强制清盘，以保留“句柄先消失、`Retain` 仍可继续使用”的既有语义。
    /// 需要释放循环引用时必须显式调用 [`Scope::close`]；关闭请求会在这里重试一次清盘。
    fn drop(&mut self) {
        let inner = self.inner_();
        if inner.decr_handle_() == 1 {
            inner.mark_closed_();
        }
        if inner.is_close_requested_() {
            inner.try_flush_();
        }
    }
}

fn scope_emplace_with_predrop_<M, P>(
    scope: &Scope,
    layout: Layout,
    emplace: M,
    predrop: P,
) -> Result<Retain<M::Target>, ScopeError>
where
    M: TrEmplace,
    P: TrPredrop<M::Target>,
{
    // SAFETY: 调用方是 `unsafe fn emplace_with_predrop` 的下游，已经承担
    // `TrEmplace::emplace` 与数据布局的安全契约。
    let result = scope
        .with_inner_(|inner| unsafe { inner.emplace_value_with_(layout, emplace, predrop) })?;
    result.map_err(|_| ScopeError::MallocFailed)
}

fn scope_put_with_factory_predrop_<Ft, Fp, P, T>(
    scope: &Scope,
    val_factory: Ft,
    predrop_factory: Fp,
) -> Result<Retain<T>, ScopeError>
where
    Ft: FnOnce() -> T,
    Fp: FnOnce() -> P,
    P: TrPredrop<T>,
{
    let value = val_factory();
    let predrop = predrop_factory();
    let layout = Layout::new::<T>();
    let emplace = IntoEmplace::<_, T>::new(
        |_layout, place: *mut T| {
            // SAFETY: place 指向 arena 按 layout 划出、尚未初始化的数据区
            unsafe { place.write(value) };
        },
        (),
    );
    // SAFETY: 由调用方满足 `TrEmplace` 的安全契约（见 trait 文档）
    unsafe { scope.emplace_with_predrop(layout, emplace, predrop) }
}

fn scope_put_str_with_predrop_<P>(
    scope: &Scope,
    str: &str,
    predrop: P,
) -> Result<Retain<ScopeStr>, ScopeError>
where
    P: TrPredrop<ScopeStr>,
{
    let len = str.len();
    let layout = Layout::for_value(str);
    // 元数据就是长度；闭包在 arena 给出的位置写入字节
    let emplace = IntoEmplace::<_, ScopeStr>::new(
        |_layout, place: *mut ScopeStr| {
            // SAFETY: place 指向 layout 划定的数据区，写入长度与 str 一致
            unsafe { core::ptr::copy_nonoverlapping(str.as_ptr(), place.cast::<u8>(), len) };
        },
        len,
    );
    // SAFETY: 写入长度与 layout 一致，且 str 不需要 Drop
    unsafe { scope.emplace_with_predrop(layout, emplace, predrop) }
}

fn scope_alloc_uninit_slice_<P, T>(
    scope: &Scope,
    length: usize,
    predrop: P,
) -> Result<Retain<[MaybeUninit<T>]>, ScopeError>
where
    P: TrPredrop<[MaybeUninit<T>]>,
{
    let layout = Layout::array::<MaybeUninit<T>>(length).map_err(|_| ScopeError::MallocFailed)?;
    // 未初始化切片不需要写入：arena 划出的数据区本身就是最终结果
    let emplace = IntoEmplace::<_, [MaybeUninit<T>]>::new(|_layout, _place| {}, length);
    // SAFETY: 数据区保持未初始化，调用方随后按契约写入
    unsafe { scope.emplace_with_predrop(layout, emplace, predrop) }
}

// ============================================================================
// 以下为重构前的旧实现，原样保留（整体注释），以便对照每个方法原本的职责。
// 注意：旧代码依赖父子 / 兄弟链与 `ScopeInner` 的 const-generic 布局，不能直接恢复。
// ============================================================================
/*
use core::{
    alloc::{Allocator, Layout},
    mem::MaybeUninit,
    ptr::{self, NonNull},
    sync::atomic::AtomicPtr,
};

#[cfg(feature = "core-alloc")]
extern crate alloc;

use crate::{
    abs_::{TrEmplace, TrScope},
    index_::Retain,
    scope_inner_::{self, RootScope, RootScopeRef},
    scope_str_::ScopeStr,
    strong_::PreDropRecord,
};

#[derive(Debug)]
pub enum ScopeError {
    /// This is rare but still possible
    MallocFailed,

    /// Error occurs during init
    MalformedInit,

    /// 目标 Scope（或其祖先）已被标记清盘，而 `strict-put-after-close` feature 要求报错。
    ///
    /// 未开启该 feature 时，关闭标记会被忽略，放入照常成功。
    ClosedScope,
}

/// 一个自包含结构，位于 Root 树，其生命周期由其分配的所有 `Retain<T>` 共同决定。
/// 即，当其分配的所有 Retain 指针都不再存活，且其所有子 Scope 也不存活，这个
/// `Scope` 的内存才会被回收。
pub struct Scope(&'static ScopeInner);

impl Scope {
    /// 从默认的 RootScope 中创建一个子 scope
    #[allow(clippy::new_without_default)]
    #[cfg(feature = "core-alloc")]
    pub fn new() -> Self {
        let root = match RootScope::try_init_default_root_scope_(
            &scope_inner_::DEFAULT_ROOT_SCOPE,
            scope_inner_::DEFAULT_PAGE_SIZE,
            alloc::alloc::Global,
        ) {
            Result::Err(s) | Result::Ok(s) => s,
        };
        // 以 root 域为父创建子 Scope；root 自身没有父域，临时句柄析构时会被跳过
        let root_scope = Self::make_scope_(root);
        Self::new_from_parent(&root_scope)
    }
}

impl Scope {
    /// 获取或者创建一个 root scope。若当前进程内已存在 root scope 则通过 Err
    /// 返回已创建的 root scope
    pub fn root<A>(
        root_ptr: &'static AtomicPtr<ScopeInner>,
        page_size: usize,
        allocator: A,
    ) -> Result<Self, Self>
    where
        A: 'static + Allocator,
    {
        let x = RootScope::try_init_default_root_scope_(
            root_ptr,
            page_size,
            allocator,
        );
        x.map(Self::make_scope_)
            .map_err(Self::make_scope_)
    }

    pub fn new_from_parent(parent: &Scope<CELL_SIZE>) -> Self {
        // SAFETY: parent.inner_ptr_ 来自一个仍然存活的 Scope
        let inner = ScopeInner::new_child_(parent.inner_ptr_).expect("分配子 Scope 失败");
        Scope { inner_ptr_: inner }
    }

    /// 显式清盘：遍历本域存活链，把仍然活着的数据逐个 `PreDrop` + 析构，再整块回收强池。
    ///
    /// 这是"清理列表"的保证来源 (b)：环、`mem::forget`、泄漏句柄导致的"永远等不到最后一个
    /// 引用释放"都由它兜底。
    ///
    /// # Safety
    ///
    /// 调用后本域所有对象的句柄（`Retain` / `Owning` / `Sharing`）都会悬空；调用方必须保证
    /// 此后不再使用它们。见 `dev-notes/weak-20260922-1135.md` §2.4。
    pub unsafe fn collect(&mut self) {
        // SAFETY: 由调用方保证清盘后不再使用本域的句柄
        let inner = unsafe { self.inner_ptr_.as_mut() };
        // SAFETY: 同上
        unsafe { inner.flush_() };
        // 顺带回收已经静默的关闭域
        inner.reclaim_pending_();
    }

    /// 创建一个子域，该子域将拥有独立的内存池和自身的生命周期。
    #[inline]
    pub fn child_scope(&self) -> Self {
        Self::new_from_parent(self)
    }

    /// 把 `factory` 造出的值就地放进本域的 arena，并交出它的句柄。
    ///
    /// 流程：从树共享的弱池取一个槽位 → 从本域强池分配一块 `StrongChunk<T>` → 就地写入
    /// 数据并登记清理信息 → 把槽位置为 `Created`、弱计数置 1 → 追加到存活链尾。
    ///
    /// # Errors
    ///
    /// 弱池已满，或强池分配失败时返回 [`ScopeError::MallocFailed`]。
    pub fn try_put<F, T>(&mut self, factory: F) -> Result<Retain<T>, ScopeError>
    where
        F: FnOnce() -> T,
    {
        self.ensure_open_()?;
        let value = factory();
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_mut() };
        inner
            .put_value_(value)
            .map_err(|_| ScopeError::MallocFailed)
    }

    /// 为类型 `T` 注册一个**类型级** `PreDrop` 钩子，作用于本 Scope 树里该类型的所有实例。
    ///
    /// 表挂在 root 上（整棵树共享一份），因此子 Scope 注册后对其父 / 兄弟同样可见。注册表
    /// 自带自旋锁，因此"多线程各自持有本树的 Scope 并注册 / 放入"不会造成数据竞争。
    /// 钩子必须是非捕获闭包或函数项；捕获式闭包会在编译期被拒绝。
    ///
    /// # Errors
    ///
    /// 本 Scope 树还没有初始化好注册表时返回 [`ScopeError::MalformedInit`]。
    pub fn set_pre_drop<T, Fin>(&mut self, pre_drop: Fin) -> Result<(), ScopeError>
    where
        T: ?Sized,
        Fin: FnOnce(&mut T) + 'static,
    {
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_ref() };
        let registry = inner.root_registry_().ok_or(ScopeError::MalformedInit)?;
        registry.register_::<T, Fin>(pre_drop);
        Result::Ok(())
    }

    /// 同 [`Scope::try_put`]，但额外传一个**对象级** `PreDrop` 钩子；它覆盖类型级钩子。
    ///
    /// # Errors
    ///
    /// 同 [`Scope::try_put`]；此外，若 `Fin` 不是零尺寸类型（捕获了环境），会在**编译期**
    /// 报错而不是运行期。
    pub fn try_put_with<F, T, Fin>(
        &mut self,
        factory: F,
        pre_drop: Fin,
    ) -> Result<Retain<T>, ScopeError>
    where
        F: FnOnce() -> T,
        Fin: FnOnce(&mut T) + 'static,
    {
        self.ensure_open_()?;
        let value = factory();
        // 钩子是零尺寸的：这里只借用它的类型作为登记键
        let record = PreDropRecord::of_with::<T, Fin>();
        core::mem::drop(pre_drop);
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_mut() };
        inner
            .put_value_with_(value, Option::Some(record))
            .map_err(|_| ScopeError::MallocFailed)
    }

    /// 把 `str` 的字节原地放进 arena，交出 [`ScopeStr`] 的句柄。
    ///
    /// 走的是 `?Sized` 的 emplace 路径：数据区布局来自 `Layout::for_value(str)`，`ScopeStr`
    /// 的元数据就是长度，因此 `&*handle` 直接得到 `&str`。
    ///
    /// # Errors
    ///
    /// 弱池已满或强池分配失败时返回 [`ScopeError::MallocFailed`]。
    pub fn try_put_str(&mut self, str: &str) -> Result<Retain<ScopeStr>, ScopeError> {
        self.ensure_open_()?;
        let len = str.len();
        let layout = Layout::for_value(str);
        // 元数据就是长度；闭包在 arena 给出的位置写入字节
        let emplace = IntoEmplace::<_, ScopeStr>::new(
            |_layout, place: *mut ScopeStr| {
                // SAFETY: place 指向 layout 划定的数据区，写入长度与 str 一致
                unsafe { core::ptr::copy_nonoverlapping(str.as_ptr(), place.cast::<u8>(), len) };
            },
            len,
        );
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_mut() };
        // SAFETY: 写入长度与 layout 一致，且 str 不需要 Drop
        unsafe { inner.emplace_value_with_(layout, emplace, Option::None) }
            .map_err(|_| ScopeError::MallocFailed)
    }

    pub fn try_clone<T>(&mut self, src: &[T]) -> Result<Retain<[T]>, ScopeError>
    where
        T: Clone,
    {
        todo!()
    }

    /// A utility method to create data unconvenient to move from construction
    /// place to the scope arena. Then use this function to emplace the data
    /// directly at the place where the memory is allocated.
    ///
    /// # Safety
    /// See `TrScope` and `TrEmplace` for safety information.
    pub unsafe fn try_emplace<TyEmp>(
        &mut self,
        layout: Layout,
        emplace: TyEmp,
    ) -> Result<Retain<TyEmp::Target>, ScopeError>
    where
        TyEmp: TrEmplace,
    {
        self.ensure_open_()?;
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_mut() };
        // SAFETY: 由调用方满足 TrEmplace::emplace 的安全契约
        unsafe { inner.emplace_value_with_(layout, emplace, Option::None) }
            .map_err(|_| ScopeError::MallocFailed)
    }

    /// 同 [`Scope::try_emplace`]，但额外传一个对象级 `PreDrop` 钩子；它覆盖类型级钩子。
    ///
    /// # Safety
    ///
    /// 同 [`Scope::try_emplace`]。
    pub unsafe fn try_emplace_with<TyEmp, Fin>(
        &mut self,
        layout: Layout,
        emplace: TyEmp,
        pre_drop: Fin,
    ) -> Result<Retain<TyEmp::Target>, ScopeError>
    where
        TyEmp: TrEmplace,
        Fin: FnOnce(&mut TyEmp::Target) + 'static,
    {
        self.ensure_open_()?;
        let record = PreDropRecord::of_with::<TyEmp::Target, Fin>();
        core::mem::drop(pre_drop);
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_mut() };
        // SAFETY: 由调用方满足 TrEmplace::emplace 的安全契约
        unsafe { inner.emplace_value_with_(layout, emplace, Option::Some(record)) }
            .map_err(|_| ScopeError::MallocFailed)
    }

    pub fn try_alloc_slice_uninit<T>(
        &mut self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, ScopeError> {
        todo!()
    }

    /// 关闭后仍放入的行为：默认忽略标记；开启 `strict-put-after-close` 时返回错误。
    fn ensure_open_(&self) -> Result<(), ScopeError> {
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let closed = unsafe { self.inner_ptr_.as_ref() }.is_closed_();
        if !closed {
            return Result::Ok(());
        }
        #[cfg(feature = "strict-put-after-close")]
        {
            Result::Err(ScopeError::ClosedScope)
        }
        #[cfg(not(feature = "strict-put-after-close"))]
        {
            Result::Ok(())
        }
    }

    fn make_scope_(root_scope_ref: RootScopeRef) -> Self {
        Scope {
            inner_ptr_: NonNull::from(root_scope_ref),
        }
    }
}

impl TrScope for &mut Scope {
    type Err = ScopeError;

    #[inline]
    fn try_put<F, T>(self, factory: F) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce() -> T,
    {
        Scope::try_put(self, factory)
    }

    #[inline]
    fn try_put_str(self, str: &str) -> Result<Retain<ScopeStr>, Self::Err> {
        Scope::try_put_str(self, str)
    }

    #[inline]
    fn try_clone<T>(self, src: &[T]) -> Result<Retain<[T]>, Self::Err>
    where
        T: Clone,
    {
        Scope::try_clone(self, src)
    }

    #[inline]
    unsafe fn try_emplace<TyEmp>(
        self,
        layout: Layout,
        emplace: TyEmp,
    ) -> Result<Retain<TyEmp::Target>, Self::Err>
    where
        TyEmp: TrEmplace,
    {
        unsafe { Scope::try_emplace(self, layout, emplace) }
    }

    #[inline]
    unsafe fn try_emplace_with<TyEmp, Fin>(
        self,
        layout: Layout,
        emplace: TyEmp,
        pre_drop: Fin,
    ) -> Result<Retain<TyEmp::Target>, Self::Err>
    where
        TyEmp: TrEmplace,
        Fin: FnOnce(&mut TyEmp::Target) + 'static,
    {
        unsafe { Scope::try_emplace_with(self, layout, emplace, pre_drop) }
    }

    #[inline]
    fn try_alloc_slice_uninit<T>(
        self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err> {
        Scope::try_alloc_slice_uninit(self, length)
    }

    #[inline]
    fn set_pre_drop<T, Fin>(self, pre_drop: Fin) -> Result<(), Self::Err>
    where
        T: ?Sized,
        Fin: FnOnce(&mut T) + 'static,
    {
        Scope::set_pre_drop::<T, Fin>(self, pre_drop)
    }

    #[inline]
    fn try_put_with<F, T, Fin>(self, factory: F, pre_drop: Fin) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce() -> T,
        Fin: FnOnce(&mut T) + 'static,
    {
        Scope::try_put_with(self, factory, pre_drop)
    }
}

impl<const CELL_SIZE: usize> core::cmp::PartialEq for Scope<CELL_SIZE> {
    fn eq(&self, other: &Self) -> bool {
        let lhs = self.inner_ptr_.as_ptr();
        let rhs = other.inner_ptr_.as_ptr();
        ptr::eq(lhs, rhs)
    }
}

impl core::cmp::Eq for Scope {}

impl Drop for Scope {
    /// 按清盘方案 A：只把子树**标记关闭、摘链、挂进待回收名单**，再尝试回收已经静默的域。
    ///
    /// 真正的析构与内存回收发生在"句柄已析构 + 已关闭 + 静默"之后（可能很久以后），
    /// 所以 `Drop` 保持 **safe**：它不会让 `Retain` 或子 `Scope` 句柄悬空。
    fn drop(&mut self) {
        // SAFETY: Scope 持有一个有效的 ScopeInner
        let inner = unsafe { self.inner_ptr_.as_mut() };
        // 根域没有 Scope 句柄，不应走到这里；防御性跳过
        if !inner.has_parent_() {
            return;
        }
        inner.mark_handle_dropped_();
        if !inner.is_closed_() {
            // 非递归地把整棵子树标记关闭并挂进名单
            inner.close_subtree_();
        }
        inner.reclaim_pending_();
    }
}

*/

#[cfg(test)]
mod tests_ {
    use alloc::alloc::Global;
    use core::{
        alloc::{AllocError, Allocator, Layout},
        ptr::NonNull,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{Scope, scope_inner_};

    static ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static TAG_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    static GLOBAL: Global = Global;

    /// 零尺寸（ZST）探针：观察 `Retain` 最后一个句柄消失后是否真的执行了 `T::drop`。
    struct DropProbe;

    impl Drop for DropProbe {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 转发给 `Global` 并统计分配次数，用来证明 per-Scope 分配器确实被使用。
    struct CountingAlloc;

    /// 带状态的分配器，用来制造与 `CountingAlloc` / `Global` 不同尺寸的 `ScopeEntity`。
    #[derive(Default)]
    struct TaggedAlloc {
        _tag: usize,
    }

    // SAFETY: 只把分配 / 释放原样转交给 `Global`
    unsafe impl Allocator for TaggedAlloc {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            TAG_ALLOCS.fetch_add(1, Ordering::SeqCst);
            GLOBAL.allocate(layout)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            // SAFETY: 指针与布局来自上面的 allocate
            unsafe { GLOBAL.deallocate(ptr, layout) }
        }
    }

    // SAFETY: 只把分配 / 释放原样转交给 `Global`
    unsafe impl Allocator for CountingAlloc {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            ALLOCS.fetch_add(1, Ordering::SeqCst);
            GLOBAL.allocate(layout)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            // SAFETY: 指针与布局来自上面的 allocate
            unsafe { GLOBAL.deallocate(ptr, layout) }
        }
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// 测试 Scope 的 Clone 共享同一个锁实体，且每域分配器参与池分配。
    /// Test that cloned Scopes share one locked entity and the per-scope allocator is used.
    ///
    /// - 手段：用计数分配器创建一个域，克隆后分别放入两个值。
    ///   / How: create a scope with a counting allocator, clone it, and put two values.
    /// - 判断：两个句柄相等，两次放入都能读回原值，计数分配器至少被调用一次。
    ///   / What: handles compare equal, both puts read back, and the counting allocator was
    ///   called at least once.
    #[test]
    fn scope_clone_shares_locked_entity_and_uses_per_scope_allocator() {
        assert_send_sync::<Scope>();

        ALLOCS.store(0, Ordering::SeqCst);
        let scope = Scope::new_with_alloc_value::<CountingAlloc, scope_inner_::DefaultConfig<Global>>(
            CountingAlloc,
        );
        let clone = scope.clone();
        assert!(scope == clone, "克隆句柄必须指向同一个实体");

        let first = scope.put(11u32).expect("第一次放入应当成功");
        let second = clone.put(22u32).expect("第二次放入应当成功");
        assert_eq!(*first.try_owning().expect("第一个值应当可独占"), 11);
        assert_eq!(*second.try_owning().expect("第二个值应当可独占"), 22);
        assert!(
            ALLOCS.load(Ordering::SeqCst) > 0,
            "每域分配器应当参与池分配"
        );

        // 句柄析构走类型擦除路径；u32 的析构是空操作，vtable 必须能查到。
        drop(first);
        drop(second);
    }

    /// 测试多个线程并发同步放入时，所有写操作都在同一把实体自旋锁上排队。
    /// Test that concurrent synchronous puts from multiple threads are serialized by the
    /// entity spinlock.
    ///
    /// - 手段：克隆同一个 `Scope` 到 4 个线程，每个线程直接调用同步 `put` 放入一个不同的值。
    ///   / How: clone one scope into four threads and call the synchronous `put` directly in
    ///   each thread.
    /// - 判断：所有线程都成功读回自己放入的值，说明锁与句柄计数在跨线程下可用。
    ///   / What: every thread reads back its own value, showing the lock and handle count work
    ///   across threads.
    #[test]
    fn scope_lock_serializes_puts_across_threads() {
        let scope = Scope::new_with_alloc::<Global, scope_inner_::DefaultConfig<Global>>();
        let mut threads = std::vec::Vec::new();
        for round in 0..4u32 {
            let scope = scope.clone();
            threads.push(std::thread::spawn(move || {
                let retain = scope.put(round).expect("跨线程放入应当成功");
                let value = *retain.try_owning().expect("数据应当可独占");
                drop(retain);
                value
            }));
        }
        let values: std::vec::Vec<u32> = threads
            .into_iter()
            .map(|thread| thread.join().expect("线程不应 panic"))
            .collect();
        assert_eq!(values, std::vec![0, 1, 2, 3]);
    }

    /// 测试同一个 root 的强块池可以同时容纳不同 `A` 类型的域实体。
    /// Test that one root strong pool can hold scope entities with different `A` types.
    ///
    /// - 手段：在同一 root 下分别用带状态的 `TaggedAlloc` 和 `Global` 创建两个域并放入值。
    ///   / How: create two scopes under the same root, one with a stateful `TaggedAlloc` and one
    ///   with `Global`, then put a value into each.
    /// - 判断：两个域都能读回各自的值，说明异质 `ScopeEntity` 共存于 root 的强块池。
    ///   / What: both scopes read back their own values, showing heterogeneous scope entities
    ///   coexist in the root strong pool.
    #[test]
    fn root_entity_pool_holds_heterogeneous_scope_entities() {
        let tagged = Scope::new_with_alloc_value::<TaggedAlloc, scope_inner_::DefaultConfig<Global>>(
            TaggedAlloc { _tag: 7 },
        );
        let plain = Scope::new_with_alloc::<Global, scope_inner_::DefaultConfig<Global>>();
        let first = tagged.put(1u32).expect("TaggedAlloc 域放入应当成功");
        let second = plain.put(2u32).expect("Global 域放入应当成功");
        assert_eq!(*first.try_owning().expect("第一个值应当可独占"), 1);
        assert_eq!(*second.try_owning().expect("第二个值应当可独占"), 2);
        assert!(
            TAG_ALLOCS.load(Ordering::SeqCst) > 0,
            "new_with_cfg 应当使用 C::PoolAlloc"
        );
        drop(first);
        drop(second);
    }

    /// 测试 `new_with_cfg` 连续创建大量域时，root 的实体强池会按需开新池。
    /// Test that repeated `new_with_cfg` calls grow the root's entity strong pools.
    ///
    /// - 手段：用默认配置连续创建 300 个域句柄。
    ///   / How: create 300 scope handles with the default config.
    /// - 判断：所有句柄都创建成功且互不相同，说明实体 chunk 能跨多个 root 强池分配。
    ///   / What: all handles are created and distinct, showing entity chunks span multiple
    ///   root strong pools.
    #[test]
    fn new_with_cfg_creates_scopes_across_root_entity_pools() {
        let scopes: std::vec::Vec<Scope> = (0..300)
            .map(|_| Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>())
            .collect();
        assert_eq!(scopes.len(), 300);
        assert!(scopes[0] != scopes[299], "实体地址必须互不相同");
    }

    /// 测试最后一个 `Retain` 消失时执行类型擦除后的 `T::drop`。
    /// 测试 ZST（零尺寸类型）的 `Drop` 经类型擦除路径恰好执行一次。
    /// Test that a ZST (zero-sized type) with `Drop` runs its destructor exactly once through
    /// the type-erased path.
    ///
    /// - 手段：放入一个零尺寸、带 `Drop` 的 `DropProbe`，然后析构 `Retain`。
    ///   / How: put a zero-sized `DropProbe` with a `Drop` impl and then drop the `Retain`.
    /// - 判断：`DropProbe` 尺寸为 0；放入后计数为 0，`Retain` 析构后计数恰好为 1。
    ///   / What: `DropProbe` is zero-sized; the counter is 0 after the put and exactly 1 after
    ///   the Retain is dropped.
    #[test]
    fn retain_drop_runs_typed_destructor() {
        assert_eq!(core::mem::size_of::<DropProbe>(), 0, "本用例覆盖 ZST");
        DROPS.store(0, Ordering::SeqCst);
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();
        let retain = scope.put(DropProbe).expect("放入应当成功");
        assert_eq!(DROPS.load(Ordering::SeqCst), 0);
        drop(retain);
        assert_eq!(DROPS.load(Ordering::SeqCst), 1, "应当执行一次 T::drop");
    }

    /// 测试 `Owning` 与 `Sharing` 的往返：独占、共享、克隆后状态都能正确回到 `Created`。
    /// Test Owning/Sharing round-trips and that cloned Retains keep the data alive.
    ///
    /// - 手段：先 `try_owning` 改写数据并释放，再 `try_sharing`、`clone`，再克隆 `Retain`。
    ///   / How: take `try_owning`, mutate and release; then `try_sharing`, clone, and clone the
    ///   `Retain`.
    /// - 判断：每一步之后仍能重新独占 / 读取；最后一个 Retain 析构时数据恰好销毁一次。
    ///   / What: after each step the data can be read/owned again, and the final Retain drop
    ///   destroys it exactly once.
    #[test]
    fn owning_and_sharing_handles_roundtrip() {
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();
        let retain = scope.put(10u32).expect("放入应当成功");
        {
            let mut owned = retain.try_owning().expect("应当可独占");
            *owned = 20;
        }
        assert!(retain.try_owning().is_some(), "归还独占权后应能再次独占");

        let shared = retain.try_sharing().expect("应当可共享");
        assert_eq!(*shared, 20);
        let shared2 = shared.clone();
        drop(shared);
        drop(shared2);
        assert!(retain.try_owning().is_some(), "强引用释放后应回到 Created");

        let clone = retain.clone();
        drop(retain);
        assert_eq!(*clone.try_owning().expect("克隆句柄仍应可独占"), 20);
        drop(clone);
    }
}

#[cfg(test)]
mod erased_drop_tests_ {
    use alloc::{alloc::Global, string::String};
    use core::{
        alloc::Layout,
        mem::{ManuallyDrop, MaybeUninit},
        ptr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{IntoEmplace, Scope, TrScope, abs_::TrPredrop, scope_inner_};

    static SAME_TYPE_DROPS: AtomicUsize = AtomicUsize::new(0);
    static SLICE_DROPS: AtomicUsize = AtomicUsize::new(0);
    static PREDROP_HOOKS: AtomicUsize = AtomicUsize::new(0);
    static PREDROP_DROPS: AtomicUsize = AtomicUsize::new(0);
    static FIXED_DROPS: AtomicUsize = AtomicUsize::new(0);
    static DYN_DROPS: AtomicUsize = AtomicUsize::new(0);
    static CLOSE_DROPS: AtomicUsize = AtomicUsize::new(0);

    struct SameTypeProbe;

    impl Drop for SameTypeProbe {
        fn drop(&mut self) {
            SAME_TYPE_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct SliceProbe(u64);

    impl Drop for SliceProbe {
        fn drop(&mut self) {
            SLICE_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct PredropProbe;

    impl Drop for PredropProbe {
        fn drop(&mut self) {
            PREDROP_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct PredropHook;

    impl TrPredrop<PredropProbe> for PredropHook {
        fn pre_drop(self, _data: &mut PredropProbe) {
            PREDROP_HOOKS.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// 固定尺寸、非 ZST、带 16 字节对齐、内嵌堆分配字段与 `Drop` 的类型。
    #[repr(align(16))]
    struct FixedProbe {
        value: u128,
        text: String,
    }

    impl Drop for FixedProbe {
        fn drop(&mut self) {
            FIXED_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    trait DynTarget {
        fn value(&self) -> u64;
    }

    struct DynProbe {
        value: u64,
    }

    impl DynTarget for DynProbe {
        fn value(&self) -> u64 {
            self.value
        }
    }

    impl Drop for DynProbe {
        fn drop(&mut self) {
            DYN_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct CloseProbe;

    impl Drop for CloseProbe {
        fn drop(&mut self) {
            CLOSE_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// 测试同一类型的多个对象共用一条 vtable，并且每个对象恰好析构一次。
    /// Test that many objects of one type reuse a single vtable and each is dropped exactly once.
    ///
    /// - 手段：在同一个域里连续放入三个 `SameTypeProbe`，逐个析构 `Retain`。
    ///   / How: put three `SameTypeProbe` values into one scope and drop their `Retain`s one by
    ///   one.
    /// - 判断：放入后计数为 0；每析构一个句柄计数加 1，最终恰好为 3。
    ///   / What: the counter is 0 after the puts and increments once per handle drop, ending at 3.
    #[test]
    fn repeated_puts_of_same_type_drop_each_once() {
        SAME_TYPE_DROPS.store(0, Ordering::Release);
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();

        let first = scope.put(SameTypeProbe).expect("第一次放入");
        let second = scope.put(SameTypeProbe).expect("第二次放入");
        let third = scope.put(SameTypeProbe).expect("第三次放入");
        assert_eq!(SAME_TYPE_DROPS.load(Ordering::Acquire), 0);

        drop(first);
        assert_eq!(SAME_TYPE_DROPS.load(Ordering::Acquire), 1);
        drop(second);
        assert_eq!(SAME_TYPE_DROPS.load(Ordering::Acquire), 2);
        drop(third);
        assert_eq!(SAME_TYPE_DROPS.load(Ordering::Acquire), 3);
    }

    /// 测试 `?Sized` 切片的每对象 metadata 能还原数据区并逐元素析构。
    /// Test that per-object metadata for an unsized slice rebuilds the data and drops every element.
    ///
    /// - 手段：用 `IntoEmplace` 把三个 `SliceProbe` 拷进 `[SliceProbe]` 数据区，元数据为长度 3；
    ///   拿到 `Retain` 后先确认切片长度，再析构句柄。
    ///   / How: copy three `SliceProbe`s into a `[SliceProbe]` data area with length metadata 3,
    ///   check the slice length through the `Retain`, then drop the handle.
    /// - 判断：`try_owning` 得到的切片长度为 3；析构句柄前计数为 0，析构后恰好为 3。
    ///   / What: the owned slice has length 3; the counter is 0 before the drop and exactly 3
    ///   afterwards.
    #[test]
    fn unsized_slice_drop_reads_per_object_metadata() {
        SLICE_DROPS.store(0, Ordering::Release);
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();

        let len = 3usize;
        let mut src: [MaybeUninit<SliceProbe>; 3] = [const { MaybeUninit::uninit() }; 3];
        for (index, slot) in src.iter_mut().enumerate() {
            slot.write(SliceProbe(index as u64));
        }
        let layout = Layout::array::<SliceProbe>(len).expect("切片布局不应溢出");
        let emplace = IntoEmplace::<_, [SliceProbe]>::new(
            |_layout, place: *mut [SliceProbe]| {
                // SAFETY: place 指向 arena 按 layout 划出的数据区，源数组已初始化
                unsafe {
                    ptr::copy_nonoverlapping(
                        src.as_ptr().cast::<SliceProbe>(),
                        place.cast::<SliceProbe>(),
                        len,
                    )
                };
            },
            len,
        );
        // SAFETY: emplace 写入的元素个数与 layout、metadata 一致
        let retain = unsafe { scope.emplace_with_predrop(layout, emplace, ()) }
            .expect("?Sized 放入应当成功");

        assert_eq!(
            retain.try_owning().expect("切片应当可独占").len(),
            len,
            "metadata 必须还原出正确的切片长度"
        );
        assert_eq!(SLICE_DROPS.load(Ordering::Acquire), 0);
        drop(retain);
        assert_eq!(
            SLICE_DROPS.load(Ordering::Acquire),
            len,
            "切片里的每个元素都恰好析构一次"
        );
    }

    /// 测试 Predrop 类型参与 vtable 合成键，析构时先跑钩子再析构数据。
    /// Test that the Predrop type is part of the vtable key and runs before the data drop.
    ///
    /// - 手段：通过 `put_with_factory_predrop` 放入 `PredropProbe`，钩子类型为
    ///   `PredropHook`，随后析构 `Retain`。
    ///   / How: put a `PredropProbe` through `put_with_factory_predrop` with `PredropHook`
    ///   as the Predrop type, then drop the `Retain`.
    /// - 判断：析构前两个计数都是 0；析构后钩子与 `Drop` 都恰好为 1。
    ///   / What: both counters are 0 before the drop and exactly 1 for both the hook and `Drop`
    ///   afterwards.
    #[test]
    fn predrop_vtable_runs_hook_then_drop() {
        PREDROP_HOOKS.store(0, Ordering::Release);
        PREDROP_DROPS.store(0, Ordering::Release);
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();

        let retain = scope
            .put_with_factory_predrop(|| PredropProbe, || PredropHook)
            .expect("带 Predrop 的放入应当成功");
        assert_eq!(PREDROP_HOOKS.load(Ordering::Acquire), 0);
        assert_eq!(PREDROP_DROPS.load(Ordering::Acquire), 0);

        drop(retain);
        assert_eq!(PREDROP_HOOKS.load(Ordering::Acquire), 1, "Predrop 恰好一次");
        assert_eq!(PREDROP_DROPS.load(Ordering::Acquire), 1, "Drop 恰好一次");
    }

    /// 测试 `str` 的每对象长度元数据能经类型擦除路径还原为 `&str`。
    /// Test that per-object length metadata for `str` round-trips through the erased path.
    ///
    /// - 手段：通过 `put_str_with_predrop` 放入 `"hello"`，再用 `Owning` 读取。
    ///   / How: put `"hello"` through `put_str_with_predrop` and read it through `Owning`.
    /// - 判断：读出的字符串内容与长度都正确，析构句柄不会触发错误析构。
    ///   / What: the string content and length are correct, and dropping the handle does not run
    ///   a wrong destructor.
    #[test]
    fn put_str_keeps_length_metadata() {
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();

        let retain = scope
            .put_str_with_predrop("hello", ())
            .expect("放入 str 应当成功");
        let owning = retain.try_owning().expect("字符串应当可独占");
        let text: &str = &owning;
        assert_eq!(text, "hello");
        assert_eq!(text.len(), 5);
        drop(owning);
        drop(retain);
    }
    /// 测试固定尺寸、非 ZST、带较高对齐要求且实现 `Drop` 的类型。
    /// Test a fixed-size, non-ZST type with a stronger alignment requirement and a `Drop` impl.
    ///
    /// - 手段：通过同步 `put` 放入 `FixedProbe`（16 字节对齐），先用 `Owning` 读回字段，
    ///   再析构最后一个 `Retain`。
    ///   / How: put a 16-byte-aligned `FixedProbe` through synchronous `put`, read its field through
    ///   `Owning`, then drop the last `Retain`.
    /// - 判断：读回值与写入值一致；析构计数从 0 变为恰好 1。
    ///   / What: the read value matches the written one and the drop counter goes from 0 to 1.
    #[test]
    fn fixed_sized_aligned_drop_runs_once() {
        FIXED_DROPS.store(0, Ordering::Release);
        assert_eq!(
            core::mem::align_of::<FixedProbe>(),
            16,
            "本用例要覆盖高于指针宽度的对齐"
        );
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();
        let expected = 0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEFu128;

        let retain = scope
            .put(FixedProbe {
                value: expected,
                text: String::from("fixed-probe"),
            })
            .expect("固定尺寸类型放入应当成功");
        let owned = retain.try_owning().expect("固定尺寸对象应当可独占");
        assert_eq!(owned.value, expected);
        assert_eq!(owned.text, "fixed-probe");
        drop(owned);
        assert_eq!(FIXED_DROPS.load(Ordering::Acquire), 0);
        drop(retain);
        assert_eq!(FIXED_DROPS.load(Ordering::Acquire), 1, "Drop 恰好一次");
    }

    /// 测试带 vtable metadata 的 trait object DST 经类型擦除路径析构。
    /// Test dropping a trait-object DST (vtable metadata) through the erased drop path.
    ///
    /// - 手段：用 `IntoEmplace` 把一个具体 `DynProbe` 写进 `dyn DynTarget` 的数据区，
    ///   metadata 来自 `ptr::metadata`；先通过 `&dyn DynTarget` 读值，再析构 `Retain`。
    ///   / How: write a concrete `DynProbe` into a `dyn DynTarget` data area with
    ///   `IntoEmplace`, taking metadata from `ptr::metadata`; read it through the trait object,
    ///   then drop the `Retain`.
    /// - 判断：trait 方法读到原值，析构计数恰好为 1，证明 vtable metadata 能被还原并触发
    ///   具体类型的 `Drop`。
    ///   / What: the trait method reads the original value and the drop counter is exactly 1,
    ///   proving the vtable metadata is rebuilt and dispatches to the concrete `Drop`.
    #[test]
    fn trait_object_dst_drop_uses_vtable_metadata() {
        DYN_DROPS.store(0, Ordering::Release);
        let scope = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();

        let value = ManuallyDrop::new(DynProbe { value: 0xDEAD_BEEF });
        let source: &dyn DynTarget = &*value;
        let layout = Layout::for_value(source);
        let meta = ptr::metadata(source);
        let emplace = IntoEmplace::<_, dyn DynTarget>::new(
            |_layout, place: *mut dyn DynTarget| {
                // SAFETY: place 指向按具体 DynProbe 布局划出的数据区，源值由 ManuallyDrop
                // 保证不会被重复析构
                unsafe {
                    ptr::copy_nonoverlapping(
                        (&*value) as *const DynProbe,
                        place.cast::<DynProbe>(),
                        1,
                    )
                };
            },
            meta,
        );
        // SAFETY: 数据区大小/对齐来自 source，闭包写入一个完整的 DynProbe
        let retain = unsafe { scope.emplace_with_predrop(layout, emplace, ()) }
            .expect("trait object DST 放入应当成功");

        assert_eq!(
            retain
                .try_owning()
                .expect("trait object 应当可独占")
                .value(),
            0xDEAD_BEEF
        );
        assert_eq!(DYN_DROPS.load(Ordering::Acquire), 0);
        drop(retain);
        assert_eq!(
            DYN_DROPS.load(Ordering::Acquire),
            1,
            "具体类型 Drop 恰好一次"
        );
    }
    /// 测试 `close` 在 `Owning` / `Sharing` 存活期间跳过对象，释放视图后再清盘。
    /// Test that `close` skips objects with live `Owning` / `Sharing` views and retries after
    /// those views are released.
    ///
    /// - 手段：两个 Scope 各放入一个 `CloseProbe`，分别持有 `Owning` 与 `Sharing`；调用
    ///   `close` 后检查计数，再释放视图。
    ///   / How: put one `CloseProbe` into each of two Scopes, hold an `Owning` in one and a
    ///   `Sharing` in the other, call `close`, check the counter, then release the views.
    /// - 判断：视图存活时 Drop 计数为 0；释放视图后各自恰好析构一次，且不重复。
    ///   / What: the drop counter stays 0 while the views are alive, then each object is dropped
    ///   exactly once after its view is released.
    #[test]
    fn close_skips_live_access_views_until_released() {
        CLOSE_DROPS.store(0, Ordering::Release);

        let scope_owning = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();
        let retain_owning = scope_owning.put(CloseProbe).expect("Owning 用例放入");
        let owning = retain_owning.try_owning().expect("应当可独占");
        scope_owning.close();
        assert_eq!(
            CLOSE_DROPS.load(Ordering::Acquire),
            0,
            "Owning 存活期间 close 必须跳过该对象"
        );
        drop(owning);
        assert_eq!(
            CLOSE_DROPS.load(Ordering::Acquire),
            1,
            "Owning 释放后 close 的重试应析构该对象"
        );
        drop(retain_owning);
        assert_eq!(CLOSE_DROPS.load(Ordering::Acquire), 1, "不得重复析构");

        let scope_sharing = Scope::new_with_cfg::<scope_inner_::DefaultConfig<Global>>();
        let retain_sharing = scope_sharing.put(CloseProbe).expect("Sharing 用例放入");
        let sharing = retain_sharing.try_sharing().expect("应当可共享");
        scope_sharing.close();
        assert_eq!(
            CLOSE_DROPS.load(Ordering::Acquire),
            1,
            "Sharing 存活期间 close 必须跳过该对象"
        );
        drop(sharing);
        assert_eq!(
            CLOSE_DROPS.load(Ordering::Acquire),
            2,
            "Sharing 释放后 close 的重试应析构该对象"
        );
        drop(retain_sharing);
        assert_eq!(CLOSE_DROPS.load(Ordering::Acquire), 2, "不得重复析构");
    }
}
