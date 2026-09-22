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

    /// 重建 `*mut Target` 所需的类型元数据；`Sized` 时为 `()`。
    ///
    /// 这是解开"arena 要构造 `*mut Target` 才能调 `emplace`、而元数据本该由这次调用给出"
    /// 这个循环的钥匙：arena 先向 `meta_()` 要元数据、建好胖指针，再把胖指针交回 `emplace`。
    ///
    /// 约束成 `Into<Target::Metadata>` 是因为 arena 必须把元数据**存进弱槽位**，日后要按
    /// `Target::Metadata` 重建胖指针；直接取 `Target::Metadata` 时走自反的 `Into` 即可。
    type Meta: Copy + Into<<Self::Target as core::ptr::Pointee>::Metadata>;

    type Result<'f>: core::ops::Try<Output = &'f mut Self::Target>
    where
        Self: 'f;

    /// 本 emplacement 使用的 `?Sized` 元数据。
    fn meta_(&self) -> Self::Meta;

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

    /// 同 [`TrScope::try_emplace`]，但额外传一个对象级 `PreDrop` 钩子；它覆盖类型级钩子。
    ///
    /// # Safety
    ///
    /// 同 [`TrScope::try_emplace`]。
    unsafe fn try_emplace_with<TyEmp, Fin>(
        self,
        layout: Layout,
        emplace: TyEmp,
        pre_drop: Fin,
    ) -> Result<Retain<TyEmp::Target>, Self::Err>
    where
        TyEmp: TrEmplace,
        Fin: FnOnce(&mut TyEmp::Target) + 'static;

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

/// 用一个"收到胖指针后自己就地写入"的闭包实现的 [`TrEmplace`]。
///
/// 因为闭包需要 `*mut T` 才能工作，元数据必须由本对象**预先持有**并交给 arena——这正是
/// `TrEmplace::Meta` + [`TrEmplace::meta_`] 的用途。
pub struct IntoEmplace<F, T>
where
    F: FnOnce(Layout, *mut T),
    T: ?Sized,
{
    factory_: F,
    meta_: <T as core::ptr::Pointee>::Metadata,
    _use_t_: PhantomData<fn() -> T>,
}

impl<F, T> IntoEmplace<F, T>
where
    F: FnOnce(Layout, *mut T),
    T: ?Sized,
{
    /// `meta` 是重建 `*mut T` 所需的元数据（`Sized` 时传 `()`）。
    pub const fn new(factory: F, meta: <T as core::ptr::Pointee>::Metadata) -> Self {
        IntoEmplace {
            factory_: factory,
            meta_: meta,
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
    type Meta = <T as core::ptr::Pointee>::Metadata;
    type Result<'f> = Result<&'f mut T, !>
    where
        Self: 'f;

    fn meta_(&self) -> Self::Meta {
        self.meta_
    }

    unsafe fn emplace(
        self,
        layout: Layout,
        place: *mut Self::Target,
    ) {
        let IntoEmplace { factory_: f, meta_: _, _use_t_: _ } = self;
        f(layout, place)
    }
}
