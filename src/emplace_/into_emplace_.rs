use core::alloc::Layout;

pub trait TrEmplace {
    type Target: ?Sized;

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

/// 用一个"收到胖指针后自己就地写入"的闭包实现的 [`TrEmplace`]。
///
/// 因为闭包需要 `*mut T` 才能工作，元数据必须由本对象**预先持有**并交给 arena——这正是
/// `TrEmplace::Meta` + [`TrEmplace::meta_`] 的用途。
pub struct IntoEmplace<F, T>(F)
where
    F: FnOnce() -> T;

impl<F, T> IntoEmplace<F, T>
where
    F: FnOnce() -> T,
{
    /// `meta` 是重建 `*mut T` 所需的元数据（`Sized` 时传 `()`）。
    pub const fn new(factory: F) -> Self {
        IntoEmplace(factory)
    }
}

impl<F, T> TrEmplace for IntoEmplace<F, T>
where
    F: FnOnce() -> T,
{
    type Target = T;

    unsafe fn emplace(
        self,
        _: Layout,
        place: *mut Self::Target,
    ) {
        let IntoEmplace(f) = self;
        unsafe { place.write(f()) };
    }
}
