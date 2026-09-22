use core::{
    alloc::{Allocator, Layout},
    mem::MaybeUninit,
    ptr::{self, NonNull},
};

#[cfg(feature = "core-alloc")]
extern crate alloc;

use crate::{
    abs_::{IntoEmplace, TrEmplace, TrScope},
    index_::Retain,
    scope_inner_::{self, PoolIndex, RootScope, ScopeInner},
    scope_str_::ScopeStr,
    weak_::PreDropRecord,
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

/// 一个自包含结构，位于 Root 树，其生命周期由其分配的所有 Retain<T> 共同决定。
/// 即，当其分配的所有 Retain 指针都不再存活，且其所有子 Scope 也不存活，这个
/// `Scope` 的内存才会被回收。
pub struct Scope {
    inner_ptr_: NonNull<ScopeInner<>>,
}

impl Scope {
    pub fn root<A, const CELL_SIZE: usize>(
        init_cell_count: PoolIndex,
        alloc: A,
    ) -> Scope
    where
        A: Allocator,
    {
        todo!()
    }

    /// 从默认的 RootScope 中创建一个子 scope
    #[allow(clippy::new_without_default)]
    #[cfg(feature = "core-alloc")]
    pub fn new() -> Scope {
        let root = match RootScope::try_init_default_root_scope_(
            &scope_inner_::DEFAULT_ROOT_SCOPE,
            scope_inner_::DEFAULT_PAGE_SIZE,
            alloc::alloc::Global,
        ) {
            Result::Err(s) | Result::Ok(s) => s,
        };
        // 以 root 域为父创建子 Scope；root 自身没有父域，临时句柄析构时会被跳过
        let root_scope = Scope {
            inner_ptr_: NonNull::from(root.inner_()),
        };
        Self::new_from_parent(&root_scope)
    }

    pub fn new_from_parent(parent: &Scope) -> Scope {
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

    /// 创建一个子域，该子域将拥有独立的内存池和自身的生命周期。
    #[inline]
    pub fn child_scope(&self) -> Scope {
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

impl core::cmp::PartialEq for Scope {
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
