//! 集成测试：公开 API 视角的 `try_put_str` 与 `try_emplace*`。
//!
//! 覆盖 `ScopeStr` 作为 `str` 的 DST 形态，以及 `?Sized` 目标的就地构造路径。与
//! `pre_drop_.rs` 一样，用一把互斥锁串行化共享全局 root 的用例（弱池尚未做同步）。

use core::alloc::Layout;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use sw_scope::{IntoEmplace, Scope, ScopeStr, TrScope};

/// 串行化本文件里的用例：它们共享全局 root 与其弱池。
static TEST_LOCK: Mutex<()> = Mutex::new(());

static STR_HOOKED: AtomicUsize = AtomicUsize::new(0);
static EMPLACE_HOOKED: AtomicUsize = AtomicUsize::new(0);

fn str_hook(text: &mut ScopeStr) {
    assert_eq!(&**text, "hooked str", "钩子里应当看到完整内容");
    STR_HOOKED.fetch_add(1, Ordering::AcqRel);
}

/// 验证 `put_str`：`ScopeStr` 保持 `str` 的 DST 形态，句柄可直接当 `&str` 用。
/// - 手段：`scope.put_str` 放入一句话，取 `Owning` 后按 `&str` 使用。
/// - 判断：内容与长度都对；`Borrow<str>` 也给出同样的视图。
#[test]
fn put_str_is_usable_as_str() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let mut scope = Scope::new();
    let retain = scope.put_str("hello, scope");

    let owning = retain.try_owning().expect("应能独占");
    let text: &str = &owning;
    assert_eq!(text, "hello, scope");
    assert_eq!(owning.len(), 12);
    let borrowed: &str = core::borrow::Borrow::borrow(&*owning);
    assert_eq!(borrowed, "hello, scope");
}

/// 验证类型级 `PreDrop` 钩子对 `ScopeStr` 也生效，且钩子里能看到字符串内容。
/// - 手段：给 `ScopeStr` 注册类型级钩子，放入字符串后析构最后一个句柄。
/// - 判断：钩子计数为 1（钩子内部的 `assert_eq!` 同时验证了内容）。
#[test]
fn put_str_type_hook_runs_on_destruction() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    STR_HOOKED.store(0, Ordering::Release);

    let mut scope = Scope::new();
    scope
        .set_pre_drop::<ScopeStr, _>(str_hook)
        .expect("注册类型级钩子应当成功");
    let retain = scope.put_str("hooked str");
    drop(retain);

    assert_eq!(STR_HOOKED.load(Ordering::Acquire), 1);
}

/// 验证 `try_emplace` 在 `Sized` 目标上的就地构造。
/// - 手段：用 `IntoEmplace` 在 `u64` 的数据区写入 99。
/// - 判断：句柄独占后读到 99。
#[test]
fn try_emplace_constructs_sized_target() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let mut scope = Scope::new();
    let emplace =
        IntoEmplace::<_, u64>::new(|_layout, place: *mut u64| unsafe { place.write(99) }, ());
    let retain = unsafe { scope.try_emplace(Layout::new::<u64>(), emplace) }.expect("应当成功");
    assert_eq!(*retain.try_owning().expect("应能独占"), 99);
}

/// 验证 `try_emplace` 在 `?Sized`（切片）目标上也能工作。
/// - 手段：`Target = [u64]`、元数据为长度 3，在数据区写入三个元素。
/// - 判断：句柄独占后得到的切片与源数组逐元素相等。这是 `?Sized` 分配路径的关键用例，
///   也是 `try_put_str` 所依赖的机制。
#[test]
fn try_emplace_constructs_unsized_slice() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    let values: [u64; 3] = [1, 2, 3];
    let mut scope = Scope::new();
    let emplace = IntoEmplace::<_, [u64]>::new(
        |_layout, place: *mut [u64]| unsafe {
            core::ptr::copy_nonoverlapping(values.as_ptr(), place.cast::<u64>(), values.len())
        },
        values.len(),
    );
    let retain =
        unsafe { scope.try_emplace(Layout::for_value(&values[..]), emplace) }.expect("应当成功");
    let owning = retain.try_owning().expect("应能独占");
    assert_eq!(&*owning, &values[..]);
}

/// 验证 `try_emplace_with` 的对象级钩子在销毁时运行。
/// - 手段：带钩子地 emplace 一个 `u64`，析构句柄。
/// - 判断：钩子计数为 1，且钩子里读到写入的 7。
#[test]
fn try_emplace_with_runs_object_hook() {
    let _guard = TEST_LOCK.lock().expect("测试锁不该中毒");
    EMPLACE_HOOKED.store(0, Ordering::Release);

    let mut scope = Scope::new();
    let emplace =
        IntoEmplace::<_, u64>::new(|_layout, place: *mut u64| unsafe { place.write(7) }, ());
    let retain = unsafe {
        scope.try_emplace_with(Layout::new::<u64>(), emplace, |value: &mut u64| {
            assert_eq!(*value, 7);
            EMPLACE_HOOKED.fetch_add(1, Ordering::AcqRel);
        })
    }
    .expect("应当成功");
    drop(retain);

    assert_eq!(EMPLACE_HOOKED.load(Ordering::Acquire), 1);
}
