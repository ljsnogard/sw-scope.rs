//! [`ScopeStr`]：arena 里的 `str`。

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
        &self.0
    }
}

impl AsRef<str> for ScopeStr {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Debug for ScopeStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(&self.0, f)
    }
}

impl core::fmt::Display for ScopeStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}
