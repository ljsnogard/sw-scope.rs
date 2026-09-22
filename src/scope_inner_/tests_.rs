use alloc::alloc::Global;
use core::ptr::NonNull;

use super::{DEFAULT_PAGE_SIZE, DEFAULT_ROOT_SCOPE, RootExtension, RootScope, ScopeInner};

/// 测试 root 真正持有树级共享资源（弱池链、注册表），且无论空域还是有对象的域，都能 O(1)
/// 找到 root：空域走"分配器指针反推"，有对象的域再走
/// "强块 → 弱槽位 → 弱池 → root"。
/// - 手段：取（或并发初始化）默认 root，创建一个子域并放入 400 个对象，强制弱池扩容出
///   第二个池；分别在放入前后用 `ScopeInner::root_extension_`、并对存活链头尾两个槽位用
///   `RootExtension::of_weak_` 反推 root 扩展。
/// - 判断：两条路径都得到同一个扩展基址；扩展的弱池链头非空，说明弱池确实挂在 root 上
///   而不是每个域各存一份。
#[test]
fn root_scope_owns_shared_pools_and_is_reachable_from_slots() {
    let root = match RootScope::<Global, 8>::try_init_default_root_scope_(
        &DEFAULT_ROOT_SCOPE,
        DEFAULT_PAGE_SIZE,
        Global,
    ) {
        Result::Ok(root) | Result::Err(root) => root,
    };
    let root_ext = root.root_extension_();
    let root_ext_ptr: NonNull<RootExtension<8>> = NonNull::from(root_ext);
    assert!(
        root_ext.weak_pools_head_().is_some(),
        "弱池链头必须由 root 持有"
    );

    // SAFETY: root 是本树 root，创建子域不会与其它借用冲突
    let root_inner = NonNull::from(root);
    let mut child_ptr =
        unsafe { ScopeInner::<8>::new_child_(root_inner) }.expect("创建子域应当成功");
    // SAFETY: child_ptr 是刚创建、本测试独占的子域
    let child = unsafe { child_ptr.as_mut() };
    // 空域也要能找到 root：直接用分配器指针反推
    assert_eq!(
        NonNull::from(child.root_extension_()).as_ptr(),
        root_ext_ptr.as_ptr(),
        "空域经分配器反推的 root 扩展不正确",
    );

    for value in 0..400u32 {
        let retain = child.put_value_(value).expect("放入应当成功");
        core::mem::drop(retain);
    }
    assert_eq!(
        NonNull::from(child.root_extension_()).as_ptr(),
        root_ext_ptr.as_ptr()
    );

    let head = child.live_head_.expect("放入之后存活链应有头");
    let tail = child.live_tail_.expect("放入之后存活链应有尾");
    for weak in [head, tail] {
        let derived = RootExtension::<8>::of_weak_(weak);
        assert_eq!(
            derived.as_ptr(),
            root_ext_ptr.as_ptr(),
            "由槽位反推的 root 扩展与 root 基址不一致",
        );
    }
}
