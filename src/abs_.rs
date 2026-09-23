use core::mem::MaybeUninit;

use crate::{
    emplace_::{self, TrEmplace},
    scope_str_::{self, ScopeStr},
    smart_pointer_::Retain,
};

pub trait TrScope {
    type Err;

    /// Like placement new in C++, to construct big object directly in place
    /// instead of create on stack then move. This is also helpful when `T` has
    /// self-referential structure.
    ///
    /// # Safety
    /// - Caller must guarantee the emplaced value is safe for drop semantics;
    /// - Caller must guarantee the process of emplacement will not exceed the
    ///   memory border;
    unsafe fn try_emplace_with<TyEmp, TyPre>(
        self,
        emplace: TyEmp,
        pre_drop: TyPre,
    ) -> Result<Retain<TyEmp::Target>, Self::Err>
    where
        TyEmp: TrEmplace,
        TyPre: TrPreDrop<TyEmp::Target>;

    fn try_alloc_slice_uninit<T>(
        self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err>;

    /// 为类型 `T` 注册一个类型级 `PreDrop` 本 Scope 内有效的钩子。
    /// 作用于该类型的所有实例，但可能会被每对象的钩子或者子域的覆盖。
    fn set_pre_drop<P, T>(self, pre_drop: P) -> Result<(), Self::Err>
    where
        P: TrPreDrop<T>,
        T: ?Sized;
}

pub trait TrScopeExt
where
    Self: Sized + TrScope,
{
    fn try_put<F, T>(self, factory: F) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce() -> T,
    {
        let emplace = emplace_::IntoEmplace::new(factory);
        unsafe { Self::try_emplace_with(self, emplace, ()) }
    }

    fn try_put_str(self, str: &str) -> Result<Retain<ScopeStr>, Self::Err> {
        let emplace = scope_str_::EmplaceScopeStr::copying(str);
        unsafe { Self::try_emplace_with(self, emplace, ()) }
    }

    fn try_clone<T>(
        self,
        src: &[T],
    ) -> Result<Retain<[T]>, Self::Err>
    where
        T: Clone,
    {
        todo!()
    }

    /// Like placement new in C++, to construct big object directly in place
    /// instead of create on stack then move. This is also helpful when `T` has
    /// self-referential structure.
    ///
    /// # Safety
    /// - Caller must guarantee the emplaced value is safe for drop semantics;
    /// - Caller must guarantee the process of emplacement will not exceed the
    ///   memory border;
    unsafe fn try_emplace<TyEmp>(
        self,
        emplace: TyEmp,
    ) -> Result<Retain<TyEmp::Target>, Self::Err>
    where
        TyEmp: TrEmplace
    {
        todo!()
    }

    /// 同 [`TrScope::try_put`]，但额外传一个对象级 `PreDrop` 钩子；它覆盖类型级钩子。
    fn try_put_with<F, T, P>(
        self,
        factory: F,
        pre_drop: P,
    ) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce() -> T,
        P: TrPreDrop<T>,
    {
        todo!()
    }

    fn put<T>(self, data: T) -> Retain<T> where Self: Sized {
        let Result::Ok(w) = self.try_put(|| data) else {
            panic!()
        };
        w
    }

    fn put_with<F, T, P>(self, factory: F, pre_drop: P) -> Retain<T>
    where
        F: FnOnce() -> T,
        P: TrPreDrop<T>,
    {
        let Result::Ok(w) = self.try_put_with(factory, pre_drop) else {
            panic!()
        };
        w
    }

    fn put_str(self, str: &str) -> Retain<ScopeStr> where Self: Sized {
        let Result::Ok(w) = self.try_put_str(str) else {
            panic!()
        };
        w
    }
}

/// Provide the potential last access to the data in the scope its memory is
/// released by either the smart pointer or by the scope.
pub trait TrPreDrop<T: ?Sized> where Self: Sized {
    fn pre_drop(&self, data: &mut T);
}

impl<T: ?Sized> TrPreDrop<T> for () {
    fn pre_drop(&self, _: &mut T) {}
}

pub impl(crate) trait TrShareMarker
{}
