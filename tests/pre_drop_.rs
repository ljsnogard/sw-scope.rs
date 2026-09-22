//! 集成测试：从公开 API 角度验证 `PreDrop` 钩子（类型级 / 对象级、覆盖关系、跨子域共享）。
//!
//! 设计依据：`dev-notes/weak-20260922-1135.md` §5。
//!
//! 注意：`Scope::new` 走的是进程内共享的全局 root，其弱槽位池还没有同步；为了让各用例
//! 在共享池上的分配互不干扰，本文件用一把互斥锁把它们串行化。（注册表本身已经带自旋锁，
//! 见 `pre_drop_registry_is_thread_safe`。）

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

/// 清盘用例的类型级钩子与析构探针计数。
static C_HOOKED: AtomicUsize = AtomicUsize::new(0);
static C_DROPPED: AtomicUsize = AtomicUsize::new(0);

struct TypeLevelC;
impl Drop for TypeLevelC {
    fn drop(&mut self) {
        C_DROPPED.fetch_add(1, Ordering::AcqRel);
    }
}
fn hook_c(_: &mut TypeLevelC) {
    C_HOOKED.fetch_add(1, Ordering::AcqRel);
}

/// 验证清盘的保证路径：对象仍被句柄持有时，`collect()` 也会强制跑 `PreDrop` 并析构它。
/// - 手段：注册类型级钩子，放入一个实例并用 `ManuallyDrop` 扣住其句柄，然后调用 `collect()`。
/// - 判断：钩子与 `Drop` 计数都恰为 1。若没有来源 (b)，句柄被扣住就永远等不到"最后一个引用
///   释放"，两个计数都会是 0。
#[test]
fn collect_forces_pre_drop_for_still_alive_objects() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    C_HOOKED.store(0, Ordering::Release);
    C_DROPPED.store(0, Ordering::Release);

    let mut scope = Scope::new();
    scope
        .set_pre_drop::<TypeLevelC, _>(hook_c)
        .expect("注册类型级钩子应当成功");
    // 用 ManuallyDrop 扣住句柄，模拟"引用一直不放"的情形
    let _leaked = core::mem::ManuallyDrop::new(scope.put(TypeLevelC));
    assert_eq!(C_HOOKED.load(Ordering::Acquire), 0);

    // SAFETY: 清盘之后不再使用 _leaked
    unsafe { scope.collect() };

    assert_eq!(C_HOOKED.load(Ordering::Acquire), 1, "清盘必须强制跑 PreDrop");
    assert_eq!(C_DROPPED.load(Ordering::Acquire), 1, "清盘必须强制析构数据");
}

/// 验证弱池扩容：放入超过初始弱池容量的对象不会失败。
/// - 手段：一个 Scope 里连续放入 400 个 `u64`（初始弱池约 290 个槽位），保留全部句柄。
/// - 判断：全部放入成功，且首尾句柄都能再次独占并读到写入值。如果弱池不能扩容，第 293 个
///   `put` 会走 `Err` 分支而 panic。
#[test]
fn weak_pool_grows_beyond_initial_capacity() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let mut scope = Scope::new();
    let mut handles = Vec::new();
    for value in 0..400u64 {
        handles.push(scope.put(value));
    }

    assert_eq!(*handles[0].try_owning().expect("首句柄应能独占"), 0);
    assert_eq!(*handles[399].try_owning().expect("末句柄应能独占"), 399);
}

/// 验证清盘之后槽位被归还、Scope 仍可继续使用（`collect` 不是一次性操作）。
/// - 手段：先放一批对象并逐个析构句柄，调用 `collect()` 清盘，再放入一个新对象。
/// - 判断：清盘后的新对象仍能独占并读到写入值；若槽位/强池回收有误，这一步会因复用坏内存
///   而失败或崩溃。
#[test]
fn scope_is_reusable_after_collect() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let mut scope = Scope::new();
    for value in 0..64u64 {
        let retain = scope.put(value);
        drop(retain);
    }
    // SAFETY: 此时没有存活的句柄
    unsafe { scope.collect() };

    let retain = scope.put(7u64);
    assert_eq!(*retain.try_owning().expect("清盘后应仍可用"), 7);
}
