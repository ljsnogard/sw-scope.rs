use core::{alloc::Layout, marker::PhantomData, mem::MaybeUninit};

use crate::{index_::Retain, scope_str_::ScopeStr};

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

    /// Like placement new in C++, to construct big object directly in place
    /// instead of create on stack then move. This is also helpful when `T` has
    /// self-referential structure.
    ///
    /// # Safety
    /// - Caller must guarantee the emplaced value is safe for drop semantics;
    /// - Caller must guarantee the process of emplacement will not exceed the
    ///   memory border;
    unsafe fn emplace_with_predrop<M, P>(
        &self,
        layout: Layout,
        emplace: M,
        predrop: P,
    ) -> Result<Retain<M::Target>, Self::Err>
    where
        M: TrEmplace,
        P: TrPredrop<M::Target>;

    /// A convenient method calling [`TrScope::emplace_with_predrop`] without `predrop`.
    ///
    /// # Safety
    ///
    /// 同 [`TrScope::emplace_with_predrop`]：调用方必须保证就地构造不越界，且构造结果
    /// 满足析构语义。
    unsafe fn emplace<M>(&self, layout: Layout, emplace: M) -> Result<Retain<M::Target>, Self::Err>
    where
        M: TrEmplace,
    {
        // SAFETY: 由调用方保证
        unsafe { Self::emplace_with_predrop(self, layout, emplace, ()) }
    }

    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

    fn put_with_factory_predrop<Ft, Fp, P, T>(
        &self,
        val_factory: Ft,
        predrop_factory: Fp,
    ) -> Result<Retain<T>, Self::Err>
    where
        Ft: FnOnce() -> T,
        Fp: FnOnce() -> P,
        P: TrPredrop<T>;

    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

    fn put_str_with_predrop<P>(&self, str: &str, predrop: P) -> Result<Retain<ScopeStr>, Self::Err>
    where
        P: TrPredrop<ScopeStr>;

    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
    // -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

    fn alloc_uninit_slice<P, T>(
        &self,
        length: usize,
        predrop: P,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err>
    where
        P: TrPredrop<[MaybeUninit<T>]>;
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
