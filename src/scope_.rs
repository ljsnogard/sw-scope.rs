use core::{
    alloc::Allocator,
    mem::MaybeUninit,
    ptr::{self, NonNull},
};

use crate::{
    TrScope,
    index_::Retain,
    scope_inner_::{self, ScopeInner},
    scope_str_::ScopeStr,
};

#[derive(Debug)]
pub enum ScopeError {
    /// This is rare but still possible,
    OutOfMemory,

}

/// 一个自包含结构，位于 Root 树，其生命周期由其分配的所有 Retain<T> 共同决定。
/// 即，当其分配的所有 Retain 指针都不再存活，且其所有子 Scope 也不存活，这个
/// `Scope` 的内存才会被回收。
pub struct Scope {
    inner_ptr_: NonNull<ScopeInner>,
}

impl Scope {
    pub fn root<A>(min_size: usize, alloc: A) -> &'static Self
    where
        A: Allocator,
    {
        todo!()
    }

    /// 从默认的 RootScope 中创建一个子 scope
    pub fn new() -> Scope {
        todo!()
    }

    /// 创建一个子 Scope，该子域将
    pub fn child_scope(&self) -> Scope {
        todo!()
    }

    pub fn try_put<T>(&mut self, data: T) -> Result<Retain<T>, ScopeError> {
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

    pub fn try_emplace<F, T>(&mut self, emplace: F) -> Result<Retain<T>, ScopeError>
    where
        F: FnOnce(&mut MaybeUninit<T>) -> &mut T,
        T: Sized
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
    fn try_put<T>(self, data: T) -> Result<Retain<T>, Self::Err> {
        Scope::try_put(self, data)
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
    fn try_emplace<F, T>(self, emplace: F) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce(&mut MaybeUninit<T>) -> &mut T,
        T: Sized
    {
        Scope::try_emplace(self, emplace)
    }

    #[inline]
    fn try_alloc_slice_uninit<T>(
        self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err> {
        Scope::try_alloc_slice_uninit(self, length)
    }
}
