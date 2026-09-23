//! 域标志与父子 / 兄弟链（代际结构）。
//!
//! 这里的方法只操作 `flags_`、`parent_scope_`、`*_sibling_`、`*_child_` 这组紧密相关的
//! 字段，因此放在同一个文件里。

use core::ptr::NonNull;

/// 各种 Scope 组成的树的共同节点基础。这是一个手动模仿面向对象“继承”
/// 的实现方式。
///
/// RootScope parent 指针恒为空。其他恒非空。
#[repr(C)]
pub(crate) struct TreeNodeBase {
    /// 父域指针
    parent_node_: Option<NonNull<TreeNodeBase>>,

    /// 自己派生的子域双向链
    child_head_: Option<NonNull<TreeNodeBase>>,
    child_tail_: Option<NonNull<TreeNodeBase>>,
}

impl TreeNodeBase {
    pub const fn new(parent: Option<NonNull<TreeNodeBase>>) -> Self {
        TreeNodeBase {
            parent_node_: parent,
            child_head_: Option::None,
            child_tail_: Option::None,
        }
    }

    #[inline]
    pub const fn parent(&self) -> Option<NonNull<TreeNodeBase>> {
        self.parent_node_
    }
    #[inline]
    pub const fn child_head(&self) -> Option<NonNull<TreeNodeBase>> {
        self.child_head_
    }
    #[inline]
    pub const fn child_tail(&self) -> Option<NonNull<TreeNodeBase>> {
        self.child_tail_
    }

    #[inline]
    pub const fn update_child_head(
        &mut self,
        updated: Option<NonNull<TreeNodeBase>>,
    ) {
        self.child_head_ = updated;
    }
    #[inline]
    pub const fn update_child_tail(
        &mut self,
        updated: Option<NonNull<TreeNodeBase>>,
    ) {
        self.child_tail_ = updated;
    }
}
