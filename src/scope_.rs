use core::{
    alloc::{Allocator, Layout},
    mem::MaybeUninit,
    ptr::{self, NonNull},
};

#[cfg(feature = "core-alloc")]
extern crate alloc;

use crate::{
    abs_::{TrEmplace, TrScope},
    index_::Retain,
    scope_inner_::{self, PoolIndex, RootScope, ScopeInner},
    scope_str_::ScopeStr,
};

#[derive(Debug)]
pub enum ScopeError {
    /// This is rare but still possible
    MallocFailed,

    /// Error occurs during init
    MalformedInit,
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

        let x = RootScope::try_init_default_root_scope_(
            &scope_inner_::DEFAULT_ROOT_SCOPE,
            scope_inner_::DEFAULT_PAGE_SIZE,
            alloc::alloc::Global,
        );
        let root_scope_inner = match x {
            Result::Err(s) => s,
            Result::Ok(s) => s,
        };
        let root_scope = unsafe {
            let ptr = root_scope_inner
                as *const _
                as *mut ScopeInner<_>;
            Scope { inner_ptr_: NonNull::new_unchecked(ptr) }
        };
        Self::new_from_parent(&root_scope)
    }

    pub fn new_from_parent(parent: &Scope) -> Scope {
        todo!()
    }

    /// 创建一个子域，该子域将拥有独立的内存池和自身的生命周期。
    #[inline]
    pub fn child_scope(&self) -> Scope {
        Self::new_from_parent(self)
    }

    pub fn try_put<F, T>(&mut self, factory: F) -> Result<Retain<T>, ScopeError>
    where
        F: FnOnce() -> T,
    {
        todo!()
    }

    pub fn try_put_str(&mut self, str: &str) -> Result<Retain<ScopeStr>, ScopeError> {
        todo!()
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
        todo!()
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
    fn try_alloc_slice_uninit<T>(
        self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err> {
        Scope::try_alloc_slice_uninit(self, length)
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
