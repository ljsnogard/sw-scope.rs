use core::{
    alloc::Layout,
    marker::PhantomData,
    mem::MaybeUninit,
};

use crate::{
    index_::Retain,
    scope_str_::ScopeStr,
};

pub trait TrEmplace {
    type Target: ?Sized;

    type Result<'f>: core::ops::Try<Output = &'f mut Self::Target>
    where
        Self: 'f;

    /// Initialize at the place specified by the place pointer following
    /// the given layout.
    ///
    /// # Safety
    /// - Must gurantee the initialization will not exceed the memory border
    ///   specified by the layout;
    /// - The initialized result must safe for drop semantics;
    unsafe fn emplace(
        self,
        layout: Layout,
        place: *mut Self::Target,
    );
}

pub trait TrScope {
    type Err;

    fn try_put<F, T>(self, factory: F) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce() -> T;

    fn try_put_str(self, str: &str) -> Result<Retain<ScopeStr>, Self::Err>;

    fn try_clone<T>(self, src: &[T]) -> Result<Retain<[T]>, Self::Err>
    where
        T: Clone;

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
        layout: Layout,
        emplace: TyEmp,
    ) -> Result<Retain<TyEmp::Target>, Self::Err>
    where
        TyEmp: TrEmplace;

    fn try_alloc_slice_uninit<T>(
        self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err>;

    /// 为类型 `T` 注册一个类型级 `PreDrop` 钩子，作用于该类型的所有实例。
    fn set_pre_drop<T, Fin>(self, pre_drop: Fin) -> Result<(), Self::Err>
    where
        T: ?Sized,
        Fin: FnOnce(&mut T) + 'static;

    /// 同 [`TrScope::try_put`]，但额外传一个对象级 `PreDrop` 钩子；它覆盖类型级钩子。
    fn try_put_with<F, T, Fin>(self, factory: F, pre_drop: Fin) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce() -> T,
        Fin: FnOnce(&mut T) + 'static;

    fn put<T>(self, data: T) -> Retain<T> where Self: Sized {
        let Result::Ok(w) = self.try_put(|| data) else {
            panic!()
        };
        w
    }

    fn put_with<F, T, Fin>(self, factory: F, pre_drop: Fin) -> Retain<T>
    where
        Self: Sized,
        F: FnOnce() -> T,
        Fin: FnOnce(&mut T) + 'static,
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

pub struct IntoEmplace<F, T>
where
    F: FnOnce(Layout, *mut T),
    T: ?Sized,
{
    factory_: F,
    _use_t_: PhantomData<fn() -> T>,
}

impl<F, T> IntoEmplace<F, T>
where
    F: FnOnce(Layout, *mut T),
    T: ?Sized,
{
    pub const fn new(factory: F) -> Self {
        IntoEmplace {
            factory_: factory,
            _use_t_: PhantomData,
        }
    }
}

impl<F, T> TrEmplace for IntoEmplace<F, T>
where
    F: FnOnce(Layout, *mut T),
    T: ?Sized,
{
    type Target = T;
    type Result<'f> = Result<&'f mut T, !>
    where
        Self: 'f;

    unsafe fn emplace(
        self,
        layout: Layout,
        place: *mut Self::Target,
    ) {
        let IntoEmplace { factory_: f, _use_t_: _ } = self;
        f(layout, place)
    }
}
