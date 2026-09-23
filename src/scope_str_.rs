//! [`ScopeStr`]：arena 里的 `str`。

use crate::emplace_;

/// 放进 [`crate::Scope`] 的字符串，本质上是 `str` 的 scoped 版本。
///
/// 它保持 `str` 的 DST 形态，因此：
/// - `Retain<ScopeStr>` 的 `?Sized` 元数据就是长度；
/// - `&*retain` 直接就是 `&str`，可以当普通 `&str` 视图来用。
///
/// # Examples
///
/// ```ignore
/// let retain: Retain<ScopeStr> = scope.put_str("hello");
/// let own = retain.try_owning().unwrap();
/// assert_eq!(&*own, "hello");
/// ```
#[repr(transparent)]
pub struct ScopeStr(str);

impl core::ops::Deref for ScopeStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        &self.0
    }
}

impl core::borrow::Borrow<str> for ScopeStr {
    #[inline]
    fn borrow(&self) -> &str {
        core::ops::Deref::deref(self)
    }
}

impl AsRef<str> for ScopeStr {
    #[inline]
    fn as_ref(&self) -> &str {
        core::ops::Deref::deref(self)
    }
}

impl core::fmt::Debug for ScopeStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = core::ops::Deref::deref(self);
        core::fmt::Debug::fmt(s, f)
    }
}

impl core::fmt::Display for ScopeStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = core::ops::Deref::deref(self);
        core::fmt::Display::fmt(s, f)
    }
}

pub(crate) struct EmplaceScopeStr<'f>(&'f str);

impl<'f> EmplaceScopeStr<'f> {
    pub const fn copying(source: &'f str) -> Self {
        EmplaceScopeStr(source)
    }
}

impl<'f> emplace_::TrEmplace for EmplaceScopeStr<'f> {
    type Target = ScopeStr;

    unsafe fn emplace(
        self,
        layout: core::alloc::Layout,
        place: *mut Self::Target,
    ) {
        let src = self.0.as_ptr();
        let dst = place.cast::<u8>();
        let count = layout.size();
        debug_assert_eq!(count, self.0.as_bytes().len());
        // SAFETY: place 指向 layout 划定的数据区，写入长度与 str 一致
        unsafe { core::ptr::copy_nonoverlapping(src, dst, count) };
    }
}
