//! 域标志与父子 / 兄弟链（代际结构）。
//!
//! 这里的方法只操作 `flags_`、`parent_scope_`、`*_sibling_`、`*_child_` 这组紧密相关的
//! 字段，因此放在同一个文件里。

use core::{
    alloc::{AllocError, Layout},
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use super::ScopeInner;

impl<const CELL_SIZE: usize> ScopeInner<CELL_SIZE> {
    /// 标志位：该域（及其子树）已被标记清盘。
    pub(crate) const FLAG_CLOSED: usize = 1 << 0;
    /// 标志位：指向本域的 `Scope` 句柄已经析构。回收要求这一位置位（见 A 方案）。
    pub(crate) const FLAG_HANDLE_DROPPED: usize = 1 << 1;

    /// 是否有父域（没有父域的是 root，root 没有 `Scope` 句柄）。
    #[inline]
    pub(crate) const fn has_parent_(&self) -> bool {
        self.parent_scope_.is_some()
    }

    /// 是否已被标记清盘。
    #[inline]
    pub(crate) fn is_closed_(&self) -> bool {
        self.flags_.load(Ordering::Acquire) & Self::FLAG_CLOSED != 0
    }

    /// 打上"句柄已析构"标记。
    #[inline]
    pub(crate) fn mark_handle_dropped_(&self) {
        self.flags_
            .fetch_or(Self::FLAG_HANDLE_DROPPED, Ordering::AcqRel);
    }

    /// 造一个子域的内部状态；分配、就地写入与入父链由 [`ScopeInner::new_child_`] 负责。
    pub(crate) fn child_of_(parent: &Self) -> Self {
        ScopeInner {
            flags_: AtomicUsize::new(0),
            parent_scope_: Option::Some(NonNull::from(parent)),
            prev_sibling_: Option::None,
            next_sibling_: Option::None,
            first_child_: Option::None,
            last_child_: Option::None,
            pending_head_: Option::None,
            pending_tail_: Option::None,
            pending_next_: Option::None,
            chain_head_: Option::None,
            chain_tail_: Option::None,
            live_head_: Option::None,
            live_tail_: Option::None,
            allocator_: parent.allocator_,
        }
    }

    /// 用本域分配器申请并就地构造一个子域，并把它追加到父域的子链尾（O(1)）。
    ///
    /// # Errors
    ///
    /// 分配器无法提供 `ScopeInner` 所需内存时返回 [`AllocError`]。
    pub(crate) fn new_child_(parent: NonNull<Self>) -> Result<NonNull<Self>, AllocError> {
        // SAFETY: 由调用方保证 parent 有效；只取一次分配器就结束借用
        let alloc = unsafe { parent.as_ref() }.allocator_;
        let mem = alloc.allocate(Layout::new::<Self>())?;
        let inner = mem.as_ptr() as *mut u8 as *mut Self;
        // SAFETY: mem 是刚分配、尚无人共享的独占内存；parent 有效且此刻独占地链接
        unsafe {
            let parent_ref = parent.as_ref();
            inner.write(Self::child_of_(parent_ref));
            let child = NonNull::new_unchecked(inner);
            let parent_ptr = parent.as_ptr();
            match (*parent_ptr).last_child_ {
                Option::Some(last) => {
                    (*last.as_ptr()).next_sibling_ = Option::Some(child);
                    (*inner).prev_sibling_ = Option::Some(last);
                }
                Option::None => (*parent_ptr).first_child_ = Option::Some(child),
            }
            (*parent_ptr).last_child_ = Option::Some(child);
            Result::Ok(child)
        }
    }
}
