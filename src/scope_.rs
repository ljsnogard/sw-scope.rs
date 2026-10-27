//! [`Scope`]：整棵树里一个域的公开句柄。
//!
//! # 新设计（重构中）
//!
//! # 实验分支 feat/scope-lock
//!
//! - 每个 [`Scope`] 只是一个指向 `ScopeHeader` 的指针；头部首字段是
//!   `CooperativeRwLock<ScopeInner>`，因此 `ScopeInner` 内联在锁里，没有单独的堆分配；
//! - 每域一份的池分配器 `A` 内联在实体尾部，`ScopeInner::allocator_` 指向它；
//! - [`Scope`] 实现 `Clone`：锁外原子句柄计数，最后一个句柄只做关闭标记；
//! - 所有放入操作先 `write_async()` 拿锁，再调用同步的 `ScopeInner` 方法。
//!
//! 实体由 `RootScope` 的分配器分配并登记到 root 的 `scopes_` 树；当前实验分支尚未实现
//! 实体回收与 `?Sized` 元信息登记。旧实现保留在文件末尾的块注释中。

use core::{
    alloc::{Allocator, Layout},
    mem::MaybeUninit,
    ptr::NonNull,
};

use abs_cancel::{NonCancellableToken, TrCancellationToken, TrMayCancel};
use gen_mcf2::gen_may_cancel_future;

use crate::{
    abs_::{IntoEmplace, TrEmplace, TrPredrop, TrScope},
    index_::Retain,
    scope_inner_::{self, DEFAULT_PAGE_SIZE, DEFAULT_ROOT_SCOPE, RootScope, ScopeHeader},
    scope_str_::ScopeStr,
    x_deps::abs_cancel,
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

/// 一个域的公开句柄：指向 `ScopeHeader`，与其它克隆共享同一个实体。
///
/// `ScopeHeader` 首字段是锁，所以本指针也等价于实验设计里的
/// `NonNull<CooperativeRwLock<ScopeInner>>`。
pub struct Scope {
    /// 指向实体公共头部；头部首字段是锁，因此该地址也是 `ScopeLock` 的地址。
    header_: NonNull<ScopeHeader>,
}

impl Scope {
    /// 尝试以某种配置来初始化根域。
    ///
    /// 当前实验分支只保证默认 root 初始化；`TyCfg` 的接入留给后续。
    pub fn try_init_root<TyCfg>() -> bool {
        let _ = core::marker::PhantomData::<TyCfg>;
        let _ = Self::default_root_();
        true
    }

    /// 从默认的 RootScope 中创建一个子 scope。
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self::new_with_alloc::<
            alloc::alloc::Global,
            scope_inner_::DefaultConfig<alloc::alloc::Global>,
        >()
    }

    /// 用默认的 `Global` 分配器，按给定配置创建域句柄。
    pub fn new_with_cfg<C>() -> Self
    where
        C: TrScopeInnerConfig,
    {
        Self::new_with_alloc::<alloc::alloc::Global, C>()
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
        match ScopeHeader::allocate_entity_::<A>(root, alloc, page_size) {
            Result::Ok(header_) => Scope { header_ },
            Result::Err(_) => panic!("分配 ScopeEntity 失败"),
        }
    }

    pub async fn put_async<T>(&self, data: T) -> Result<Retain<T>, ScopeError> {
        scope_put_async_(self, data, NonCancellableToken::new()).await
    }

    /// 取默认 root；并发初始化时后到者拿到先到者发布的 root。
    fn default_root_() -> &'static RootScope {
        match RootScope::try_init_default_root_scope_(
            &DEFAULT_ROOT_SCOPE,
            DEFAULT_PAGE_SIZE,
            alloc::alloc::Global,
        ) {
            Result::Ok(root) | Result::Err(root) => root,
        }
    }

    /// 实体公共头部。
    fn header_(&self) -> &ScopeHeader {
        // SAFETY: 实体由 'static root 持有，且在句柄存活期间不会被释放
        unsafe { self.header_.as_ref() }
    }
}

impl Scope {
    /// 在同一个 root 下创建 `parent` 的子域。
    pub fn new_from_parent(_parent: &Scope) -> Self {
        // 实验分支暂未在树中表达父子关系：先在同一 root 下创建一个独立域。
        Self::new()
    }

    /// 显式清盘：遍历本域存活链，把仍然活着的数据逐个 `PreDrop` + 析构，再整块回收强池。
    ///
    /// 这是"清理列表"的保证来源 (b)：环、`mem::forget`、泄漏句柄导致的"永远等不到最后一个
    /// 引用释放"都由它兜底。
    ///
    /// # Safety
    ///
    /// 调用后本域所有对象的句柄（`Retain` / `Owning` / `Sharing`）都会悬空；调用方必须保证
    /// 此后不再使用它们。
    pub unsafe fn collect(&mut self) {
        // 旧实现：
        //
        // // SAFETY: 由调用方保证清盘后不再使用本域的句柄
        // let inner = unsafe { self.inner_ptr_.as_mut() };
        // // SAFETY: 同上
        // unsafe { inner.flush_() };
        // // 顺带回收已经静默的关闭域
        // inner.reclaim_pending_();
        todo!()
    }

    /// 创建一个子域，该子域将拥有独立的内存池和自身的生命周期。
    #[inline]
    pub fn child_scope(&self) -> Self {
        Self::new_from_parent(self)
    }

    /// 把 `factory` 造出的值就地放进本域的 arena，并交出它的句柄。
    ///
    /// # Errors
    ///
    /// 弱池已满，或强池分配失败时返回 [`ScopeError::MallocFailed`]。
    pub fn try_put<F, T>(&mut self, _factory: F) -> Result<Retain<T>, ScopeError>
    where
        F: FnOnce() -> T,
    {
        // 旧实现：
        //
        // self.ensure_open_()?;
        // let value = factory();
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let inner = unsafe { self.inner_ptr_.as_mut() };
        // inner
        //     .put_value_(value)
        //     .map_err(|_| ScopeError::MallocFailed)
        todo!()
    }

    /// 为类型 `T` 注册一个**类型级** `PreDrop` 钩子，作用于本 Scope 树里该类型的所有实例。
    ///
    /// # Errors
    ///
    /// 本 Scope 树还没有初始化好注册表时返回 [`ScopeError::MalformedInit`]。
    pub fn set_pre_drop<T, Fin>(&mut self, _pre_drop: Fin) -> Result<(), ScopeError>
    where
        T: ?Sized,
        Fin: FnOnce(&mut T) + 'static,
    {
        // 旧实现直接经 `ScopeInner::root_registry_()` 拿 root 的注册表；新设计里注册表
        // 已经是 `RootScope` 的字段，应当通过 `self.root_.pre_drop_registry_()` 访问。
        //
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let inner = unsafe { self.inner_ptr_.as_ref() };
        // let registry = inner.root_registry_().ok_or(ScopeError::MalformedInit)?;
        // registry.register_::<T, Fin>(pre_drop);
        // Result::Ok(())
        todo!()
    }

    /// 同 [`Scope::try_put`]，但额外传一个**对象级** `PreDrop` 钩子；它覆盖类型级钩子。
    ///
    /// # Errors
    ///
    /// 同 [`Scope::try_put`]；此外，若 `Fin` 不是零尺寸类型（捕获了环境），会在**编译期**
    /// 报错而不是运行期。
    pub fn try_put_with<F, T, Fin>(
        &mut self,
        _factory: F,
        _pre_drop: Fin,
    ) -> Result<Retain<T>, ScopeError>
    where
        F: FnOnce() -> T,
        Fin: FnOnce(&mut T) + 'static,
    {
        // 旧实现：
        //
        // self.ensure_open_()?;
        // let value = factory();
        // let record = PreDropRecord::of_with::<T, Fin>();
        // core::mem::drop(pre_drop);
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let inner = unsafe { self.inner_ptr_.as_mut() };
        // inner
        //     .put_value_with_(value, Option::Some(record))
        //     .map_err(|_| ScopeError::MallocFailed)
        todo!()
    }

    /// 把 `str` 的字节原地放进 arena，交出 [`ScopeStr`] 的句柄。
    ///
    /// # Errors
    ///
    /// 弱池已满或强池分配失败时返回 [`ScopeError::MallocFailed`]。
    pub fn try_put_str(&mut self, _str: &str) -> Result<Retain<ScopeStr>, ScopeError> {
        // 旧实现（`?Sized` emplace 路径）：
        //
        // self.ensure_open_()?;
        // let len = str.len();
        // let layout = Layout::for_value(str);
        // let emplace = IntoEmplace::<_, ScopeStr>::new(
        //     |_layout, place: *mut ScopeStr| {
        //         unsafe {
        //             core::ptr::copy_nonoverlapping(str.as_ptr(), place.cast::<u8>(), len)
        //         };
        //     },
        //     len,
        // );
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let inner = unsafe { self.inner_ptr_.as_mut() };
        // unsafe { inner.emplace_value_with_(layout, emplace, Option::None) }
        //     .map_err(|_| ScopeError::MallocFailed)
        todo!()
    }

    /// 把切片 `src` 克隆进 arena。
    pub fn try_clone<T>(&mut self, _src: &[T]) -> Result<Retain<[T]>, ScopeError>
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
        _layout: Layout,
        _emplace: TyEmp,
    ) -> Result<Retain<TyEmp::Target>, ScopeError>
    where
        TyEmp: TrEmplace,
    {
        // 旧实现：
        //
        // self.ensure_open_()?;
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let inner = unsafe { self.inner_ptr_.as_mut() };
        // // SAFETY: 由调用方满足 TrEmplace::emplace 的安全契约
        // unsafe { inner.emplace_value_with_(layout, emplace, Option::None) }
        //     .map_err(|_| ScopeError::MallocFailed)
        todo!()
    }

    /// 同 [`Scope::try_emplace`]，但额外传一个对象级 `PreDrop` 钩子；它覆盖类型级钩子。
    ///
    /// # Safety
    ///
    /// 同 [`Scope::try_emplace`]。
    pub unsafe fn try_emplace_with<TyEmp, Fin>(
        &mut self,
        _layout: Layout,
        _emplace: TyEmp,
        _pre_drop: Fin,
    ) -> Result<Retain<TyEmp::Target>, ScopeError>
    where
        TyEmp: TrEmplace,
        Fin: FnOnce(&mut TyEmp::Target) + 'static,
    {
        // 旧实现：
        //
        // self.ensure_open_()?;
        // let record = PreDropRecord::of_with::<TyEmp::Target, Fin>();
        // core::mem::drop(pre_drop);
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let inner = unsafe { self.inner_ptr_.as_mut() };
        // // SAFETY: 由调用方满足 TrEmplace::emplace 的安全契约
        // unsafe { inner.emplace_value_with_(layout, emplace, Option::Some(record)) }
        //     .map_err(|_| ScopeError::MallocFailed)
        todo!()
    }

    /// 分配一段未初始化的切片。
    pub fn try_alloc_slice_uninit<T>(
        &mut self,
        _length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, ScopeError> {
        todo!()
    }

    /// 关闭后仍放入的行为：默认忽略标记；开启 `strict-put-after-close` 时返回错误。
    fn ensure_open_(&self) -> Result<(), ScopeError> {
        // 旧实现：
        //
        // // SAFETY: Scope 持有一个有效的 ScopeInner
        // let closed = unsafe { self.inner_ptr_.as_ref() }.is_closed_();
        // if !closed {
        //     return Result::Ok(());
        // }
        // #[cfg(feature = "strict-put-after-close")]
        // {
        //     Result::Err(ScopeError::ClosedScope)
        // }
        // #[cfg(not(feature = "strict-put-after-close"))]
        // {
        //     Result::Ok(())
        // }
        todo!()
    }

    /// 用 root 与键构造一个句柄。
    fn make_scope_() -> Self {
        todo!()
    }
}

impl TrScope for Scope {
    type Err = ScopeError;

    type EmplaceWithPredropAsync<'f, M, P>
        = ScopeEmplaceWithPredropAsync<'f, 'f, M, P>
    where
        M: 'f + TrEmplace,
        P: 'f + TrPredrop<M::Target>,
        Self: 'f;

    unsafe fn emplace_with_predrop_async<'f, M, P>(
        &'f self,
        layout: Layout,
        emplace: M,
        predrop: P,
    ) -> Self::EmplaceWithPredropAsync<'f, M, P>
    where
        M: 'f + TrEmplace,
        P: 'f + TrPredrop<M::Target>,
    {
        ScopeEmplaceWithPredropAsync::new(self, layout, emplace, predrop)
    }

    type PutWithFactoryPredropAsync<'f, Ft, Fp, P, T>
        = ScopePutWithFactoryPredropAsync<'f, 'f, Ft, Fp, P, T>
    where
        Ft: 'f + FnOnce() -> T,
        Fp: 'f + FnOnce() -> P,
        P: 'f + TrPredrop<T>,
        T: 'f,
        Self: 'f;

    fn put_with_factory_predrop_async<'f, Ft, Fp, P, T>(
        &'f self,
        val_factory: Ft,
        predrop_factory: Fp,
    ) -> Self::PutWithFactoryPredropAsync<'f, Ft, Fp, P, T>
    where
        Ft: FnOnce() -> T,
        Fp: FnOnce() -> P,
        P: TrPredrop<T>,
    {
        ScopePutWithFactoryPredropAsync::new(self, val_factory, predrop_factory)
    }

    type PutStrAsync<'f, P>
        = ScopePutStrWithPredropAsync<'f, 'f, P>
    where
        P: 'f + TrPredrop<ScopeStr>,
        Self: 'f;

    fn put_str_with_predrop_async<'f, P>(
        &'f self,
        str: &'f str,
        predrop: P,
    ) -> Self::PutStrAsync<'f, P>
    where
        P: TrPredrop<ScopeStr>,
    {
        ScopePutStrWithPredropAsync::new(self, str, predrop)
    }

    type AllocUninitSliceAsync<'f, P, T>
        = ScopeAllocUninitSliceAsync<'f, 'f, P, T>
    where
        P: 'f + TrPredrop<[MaybeUninit<T>]>,
        T: 'f,
        Self: 'f;

    fn alloc_uninit_slice_async<'f, P, T>(
        &'f self,
        length: usize,
        predrop: P,
    ) -> Self::AllocUninitSliceAsync<'f, P, T>
    where
        P: TrPredrop<[MaybeUninit<T>]>,
    {
        ScopeAllocUninitSliceAsync::new(self, length, predrop)
    }
}

impl core::cmp::PartialEq for Scope {
    fn eq(&self, other: &Self) -> bool {
        self.header_ == other.header_
    }
}

impl core::cmp::Eq for Scope {}

impl Clone for Scope {
    fn clone(&self) -> Self {
        self.header_().incr_handle_();
        Scope {
            header_: self.header_,
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
    /// 实验分支暂不回收实体：`Retain` 可能仍然指向实体拥有的弱槽位，而回收需要与
    /// “最后一个 Retain 释放”以及 root 的待回收队列协调。实体本身由 root 持有，
    /// 这里先保留，后续接入清盘 / 回收路径。
    fn drop(&mut self) {
        let header = self.header_();
        if header.decr_handle_() == 1 {
            header.mark_closed_();
        }
    }
}

#[gen_may_cancel_future(ScopeEmplaceWithPredrop, pub, new(pub(crate)))]
async fn scope_emplace_with_predrop_async<'f, M, P, K>(
    scope: &'f Scope,
    layout: Layout,
    emplace: M,
    predrop: P,
    cancel: K,
) -> Result<Retain<M::Target>, ScopeError>
where
    M: 'f + TrEmplace,
    P: 'f + TrPredrop<M::Target>,
    K: TrCancellationToken,
{
    Result::Err(ScopeError::ClosedScope)
}

#[gen_may_cancel_future(ScopePut)]
async fn scope_put_async_<'f, T, K>(
    scope: &'f Scope,
    value: T,
    cancel: K,
) -> Result<Retain<T>, ScopeError>
where
    T: 'f,
    K: TrCancellationToken,
{
    if scope.header_().is_closed_() {
        return Result::Err(ScopeError::ClosedScope);
    }
    let mut sess = scope.header_().lock_().acquire_session();
    let mut guard = sess
        .write_async()
        .may_cancel_with(cancel)
        .await
        // TODO(实验): 锁错误（Cancelled / Poisoned）还没有独立的公开错误变体
        .map_err(|_| ScopeError::ClosedScope)?;
    guard
        .put_value_(value)
        .map_err(|_| ScopeError::MallocFailed)
}

#[gen_may_cancel_future(ScopePutWithFactoryPredrop, pub, new(pub(crate)))]
async fn scope_put_with_factory_predrop_async<'f, Ft, Fp, P, T, K>(
    scope: &'f Scope,
    val_factory: Ft,
    predrop_factory: Fp,
    cancel: K,
) -> Result<Retain<T>, ScopeError>
where
    Ft: 'f + FnOnce() -> T,
    Fp: 'f + FnOnce() -> P,
    P: 'f + TrPredrop<T>,
    T: 'f,
    K: TrCancellationToken,
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
    unsafe { scope.emplace_with_predrop_async(layout, emplace, predrop) }
        .may_cancel_with(cancel)
        .await
}

#[gen_may_cancel_future(ScopePutStrWithPredrop, pub, new(pub(crate)))]
async fn scope_put_str_with_predrop_async<'f, P, K>(
    scope: &'f Scope,
    str: &'f str,
    predrop: P,
    cancel: K,
) -> Result<Retain<ScopeStr>, ScopeError>
where
    P: 'f + TrPredrop<ScopeStr>,
    K: TrCancellationToken,
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
    unsafe { scope.emplace_with_predrop_async(layout, emplace, predrop) }
        .may_cancel_with(cancel)
        .await
}

#[gen_may_cancel_future(ScopeAllocUninitSlice, pub, new(pub(crate)))]
async fn scope_alloc_uninit_slice_async<'f, P, T, K>(
    scope: &'f Scope,
    length: usize,
    predrop: P,
    cancel: K,
) -> Result<Retain<[MaybeUninit<T>]>, ScopeError>
where
    P: 'f + TrPredrop<[MaybeUninit<T>]>,
    T: 'f,
    K: TrCancellationToken,
{
    let layout = Layout::array::<MaybeUninit<T>>(length).map_err(|_| ScopeError::MallocFailed)?;
    // 未初始化切片不需要写入：arena 划出的数据区本身就是最终结果
    let emplace = IntoEmplace::<_, [MaybeUninit<T>]>::new(|_layout, _place| {}, length);
    // SAFETY: 数据区保持未初始化，调用方随后按契约写入
    unsafe { scope.emplace_with_predrop_async(layout, emplace, predrop) }
        .may_cancel_with(cancel)
        .await
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
        mem,
        ptr::NonNull,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use crate::{Scope, scope_inner_};

    static ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static GLOBAL: Global = Global;

    /// 转发给 `Global` 并统计分配次数，用来证明 per-Scope 分配器确实被使用。
    struct CountingAlloc;

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
    #[compio::test]
    async fn scope_clone_shares_locked_entity_and_uses_per_scope_allocator() {
        assert_send_sync::<Scope>();

        ALLOCS.store(0, Ordering::SeqCst);
        let scope = Scope::new_with_alloc_value::<CountingAlloc, scope_inner_::DefaultConfig<Global>>(
            CountingAlloc,
        );
        let clone = scope.clone();
        assert!(scope == clone, "克隆句柄必须指向同一个实体");

        let first = scope.put_async(11u32).await.expect("第一次放入应当成功");
        let second = clone.put_async(22u32).await.expect("第二次放入应当成功");
        assert_eq!(*first.try_owning().expect("第一个值应当可独占"), 11);
        assert_eq!(*second.try_owning().expect("第二个值应当可独占"), 22);
        assert!(
            ALLOCS.load(Ordering::SeqCst) > 0,
            "每域分配器应当参与池分配"
        );

        // 类型擦除析构尚未接通，先避免 `Retain::drop` 走到 `todo!()`
        mem::forget(first);
        mem::forget(second);
    }

    /// 测试多个线程通过各自的 Runtime 并发放入时，所有写操作都在同一把实体锁上排队。
    /// Test that concurrent puts from multiple threads are serialized by the entity lock.
    ///
    /// - 手段：克隆同一个 `Scope` 到 4 个线程，每个线程建自己的 compio Runtime 并放入
    ///   一个不同的值。
    ///   / How: clone one scope into four threads, each with its own compio Runtime, and put
    ///   a distinct value.
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
                let runtime = compio::runtime::Runtime::new().expect("创建 Runtime 应当成功");
                runtime.block_on(async move {
                    let retain = scope.put_async(round).await.expect("跨线程放入应当成功");
                    let value = *retain.try_owning().expect("数据应当可独占");
                    // 类型擦除析构尚未接通
                    mem::forget(retain);
                    value
                })
            }));
        }
        let values: std::vec::Vec<u32> = threads
            .into_iter()
            .map(|thread| thread.join().expect("线程不应 panic"))
            .collect();
        assert_eq!(values, std::vec![0, 1, 2, 3]);
    }
}
