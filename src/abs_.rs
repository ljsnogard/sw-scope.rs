use core::{alloc::Layout, marker::PhantomData, mem::MaybeUninit};

use abs_cancel::TrMayCancel;

use crate::{index_::Retain, scope_str_::ScopeStr, x_deps::abs_cancel};

pub trait TrPredrop<T>
where
    T: ?Sized,
{
    /// 是否是"没有 predrop 钩子"的占位值 `()`。
    ///
    /// 强块用这个常量决定是否在 `StrongChunkBase` 的 `refc_` 里置 predrop 标志位。
    /// 真实钩子（函数项、闭包、函数指针）使用默认值 `false`；`()` 覆盖为 `true`。
    const IS_ABSENT: bool = false;

    fn pre_drop(self, data: &mut T);
}

impl<F, T> TrPredrop<T> for F
where
    F: FnOnce(&mut T),
    T: ?Sized,
{
    fn pre_drop(self, data: &mut T) {
        let f = self;
        f(data)
    }
}

impl<T> TrPredrop<T> for ()
where
    T: ?Sized,
{
    const IS_ABSENT: bool = true;

    fn pre_drop(self, data: &mut T) {
        let _ = data;
    }
}

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
    unsafe fn emplace(self, layout: Layout, place: *mut Self::Target);
}

pub trait TrScope {
    type Err;

    type EmplaceWithPredropAsync<'f, M, P>: TrMayCancel<'f, MayCancelOutput = Result<Retain<M::Target>, Self::Err>>
    where
        M: 'f + TrEmplace,
        P: 'f + TrPredrop<M::Target>,
        Self: 'f;

    /// Like placement new in C++, to construct big object directly in place
    /// instead of create on stack then move. This is also helpful when `T` has
    /// self-referential structure.
    ///
    /// # Safety
    /// - Caller must guarantee the emplaced value is safe for drop semantics;
    /// - Caller must guarantee the process of emplacement will not exceed the
    ///   memory border;
    unsafe fn emplace_with_predrop_async<'f, M, P>(
        &'f self,
        layout: Layout,
        emplace: M,
        predrop: P,
    ) -> Self::EmplaceWithPredropAsync<'f, M, P>
    where
        M: 'f + TrEmplace,
        P: 'f + TrPredrop<M::Target>;

    /// A convenient method calling `try_emplace_with` without `pre_drop`.
    ///
    /// # Safety
    ///
    /// 同 [`TrScope::emplace_with_predrop_async`]：调用方必须保证就地构造不越界，且构造
    /// 结果满足析构语义。
    unsafe fn emplace_async<'f, M>(
        &'f self,
        layout: Layout,
        emplace: M,
    ) -> Self::EmplaceWithPredropAsync<'f, M, ()>
    where
        M: TrEmplace,
    {
        unsafe { Self::emplace_with_predrop_async(self, layout, emplace, ()) }
    }

    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

    type PutWithFactoryPredropAsync<'f, Ft, Fp, P, T>: TrMayCancel<'f, MayCancelOutput = Result<Retain<T>, Self::Err>>
    where
        Ft: FnOnce() -> T,
        Fp: FnOnce() -> P,
        P: TrPredrop<T>,
        Ft: 'f,
        Fp: 'f,
        P: 'f,
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
        P: TrPredrop<T>;

    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

    type PutStrAsync<'f, P>: TrMayCancel<'f, MayCancelOutput = Result<Retain<ScopeStr>, Self::Err>>
    where
        P: TrPredrop<ScopeStr>,
        P: 'f,
        Self: 'f;

    fn put_str_with_predrop_async<'f, P>(
        &'f self,
        str: &'f str,
        predrop: P,
    ) -> Self::PutStrAsync<'f, P>
    where
        P: TrPredrop<ScopeStr>;

    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

    type AllocUninitSliceAsync<'f, P, T>: TrMayCancel<'f, MayCancelOutput = Result<Retain<[MaybeUninit<T>]>, Self::Err>>
    where
        P: TrPredrop<[MaybeUninit<T>]>,
        P: 'f,
        T: 'f,
        Self: 'f;

    fn alloc_uninit_slice_async<'f, P, T>(
        &'f self,
        length: usize,
        predrop: P,
    ) -> Self::AllocUninitSliceAsync<'f, P, T>
    where
        P: TrPredrop<[MaybeUninit<T>]>;
}

pub trait TrScopeExt
where
    Self: TrScope,
{
    async fn put_async<T>(&self, _data: T) -> Result<Retain<T>, Self::Err> {
        todo!()
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
    type Result<'f>
        = Result<&'f mut T, !>
    where
        Self: 'f;

    fn meta_(&self) -> Self::Meta {
        self.meta_
    }

    unsafe fn emplace(self, layout: Layout, place: *mut Self::Target) {
        let IntoEmplace {
            factory_: f,
            meta_: _,
            _use_t_: _,
        } = self;
        f(layout, place)
    }
}
