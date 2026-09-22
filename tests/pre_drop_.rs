//! 集成测试：从公开 API 角度验证 `PreDrop` 钩子（类型级 / 对象级、覆盖关系、跨子域共享）。
//!
//! 设计依据：`dev-notes/weak-20260922-1135.md` §5。
//!
//! 注意：类型级注册表挂在全局 root 上；为了让"共享同一棵树"的用例互不干扰，本文件用一把
//! 互斥锁把各用例串行化（注册表本身的可变访问目前还没有锁，见 dev-notes §7.3）。

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use sw_scope::{Scope, TrScope};

/// 串行化本文件里的用例：它们共享全局 root 与其注册表。
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 类型级钩子计数（用例一）。
static A_HOOKED: AtomicUsize = AtomicUsize::new(0);
/// 类型级钩子计数（用例三）。
static B_HOOKED: AtomicUsize = AtomicUsize::new(0);
/// 被对象级覆盖的类型级钩子计数（用例二）。
static OVERRIDDEN_TYPE_HOOKED: AtomicUsize = AtomicUsize::new(0);
/// 对象级钩子计数（用例二）。
static OBJ_HOOKED: AtomicUsize = AtomicUsize::new(0);

struct TypeLevelA;
fn hook_a(_: &mut TypeLevelA) {
    A_HOOKED.fetch_add(1, Ordering::AcqRel);
}

struct TypeLevelB;
fn hook_b(_: &mut TypeLevelB) {
    B_HOOKED.fetch_add(1, Ordering::AcqRel);
}

struct Override;
fn type_hook_override(_: &mut Override) {
    OVERRIDDEN_TYPE_HOOKED.fetch_add(1, Ordering::AcqRel);
}
fn obj_hook(_: &mut Override) {
    OBJ_HOOKED.fetch_add(1, Ordering::AcqRel);
}

/// 验证类型级钩子：注册后该类型的所有实例在销毁时都会跑到钩子。
/// - 手段：`Scope::new` + `set_pre_drop::<TypeLevelA>`，放入一个实例，先读计数、再析构句柄。
/// - 判断：持有句柄时计数为 0；析构最后一个句柄后计数恰为 1。
#[test]
fn type_level_hook_runs_on_destruction() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    A_HOOKED.store(0, Ordering::Release);

    let mut scope = Scope::new();
    scope
        .set_pre_drop::<TypeLevelA, _>(hook_a)
        .expect("注册类型级钩子应当成功");

    let retain = scope.put(TypeLevelA);
    assert_eq!(
        A_HOOKED.load(Ordering::Acquire),
        0,
        "还持有句柄时不应当调用钩子"
    );
    drop(retain);
    assert_eq!(
        A_HOOKED.load(Ordering::Acquire),
        1,
        "最后一个句柄消失时必须调用钩子"
    );
}

/// 验证对象级钩子覆盖类型级：同一类型下，带对象级钩子的对象只跑对象级。
/// - 手段：给 `Override` 注册类型级钩子；放两个实例，一个走 `put`，一个走 `put_with`。
/// - 判断：两个都析构后，类型级计数为 1（只有未被覆盖的那个），对象级计数为 1。
#[test]
fn object_level_hook_overrides_type_level() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    OVERRIDDEN_TYPE_HOOKED.store(0, Ordering::Release);
    OBJ_HOOKED.store(0, Ordering::Release);

    let mut scope = Scope::new();
    scope
        .set_pre_drop::<Override, _>(type_hook_override)
        .expect("注册类型级钩子应当成功");

    let plain = scope.put(Override);
    let overridden = scope.put_with(|| Override, obj_hook);
    drop(plain);
    drop(overridden);

    assert_eq!(
        OVERRIDDEN_TYPE_HOOKED.load(Ordering::Acquire),
        1,
        "未被覆盖的对象应当走类型级钩子"
    );
    assert_eq!(
        OBJ_HOOKED.load(Ordering::Acquire),
        1,
        "被覆盖的对象应当走对象级钩子"
    );
}

/// 验证注册表在本 Scope 树里共享：子域注册的钩子对父域放入的对象同样生效。
/// - 手段：`Scope::new` 造父域，`child_scope()` 造子域；在子域注册钩子，在父域放入实例。
/// - 判断：析构句柄后类型级计数为 1。
#[test]
fn registry_is_shared_across_child_scopes() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    B_HOOKED.store(0, Ordering::Release);

    let mut parent = Scope::new();
    let mut child = parent.child_scope();
    child
        .set_pre_drop::<TypeLevelB, _>(hook_b)
        .expect("子域注册应当成功");

    let retain = parent.put(TypeLevelB);
    drop(retain);
    assert_eq!(
        B_HOOKED.load(Ordering::Acquire),
        1,
        "子域注册的类型级钩子对父域对象也必须生效"
    );
}
