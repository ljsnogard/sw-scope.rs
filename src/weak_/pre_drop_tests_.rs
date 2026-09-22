//! `PreDropRecord` / `PreDropRegistry` 的单元测试。
//!
//! 对应 `dev-notes/weak-20260922-1135.md` §7 列出的"实验要回答的问题"：记录是否每类型一份、
//! 类型级注册是否命中、不同类型是否串味、对象级是否覆盖类型级、以及 `PreDrop` 与 `Drop`
//! 的次序与次数。

use core::{
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::string::String;

use super::{PreDropRecord, PreDropRegistry, WeakChunk, resolve_};
use crate::strong_::StrongChunk;

/// 按值收下一个零尺寸钩子，只用它的类型取登记；用于在测试里推断 `Fin`。
fn record_with_<T: ?Sized, Fin: FnOnce(&mut T) + 'static>(_hook: Fin) -> &'static PreDropRecord {
    PreDropRecord::of_with::<T, Fin>()
}

/// 验证"每类型一份"：同一 `T` 的两次登记必须取到同一个静态地址。
/// - 手段：对 `String` 两次调用 `PreDropRecord::of::<String>()`，并各取一份 `u64` 的记录。
/// - 判断：`String` 两次结果地址相等且非空操作；`u64` 不需要 `Drop`，其记录是空操作。
#[test]
fn pre_drop_record_is_shared_per_type() {
    assert!(
        ptr::eq(
            PreDropRecord::of::<String>(),
            PreDropRecord::of::<String>()
        ),
        "同一类型必须共用同一份登记"
    );
    assert!(
        !PreDropRecord::of::<String>().is_noop_(),
        "需要 Drop 的类型必须有清理入口"
    );
    assert!(
        PreDropRecord::of::<u64>().is_noop_(),
        "既不需要 Drop 也没有钩子的类型应当是空操作"
    );
}

/// 验证类型级注册能命中被注册的类型。
/// - 手段：在空注册表上给 `u64` 注册一个钩子，然后查表。
/// - 判断：`lookup_::<u64>` 命中且不是空操作。
#[test]
fn pre_drop_registry_hits_registered_type() {
    let registry = PreDropRegistry::new();
    registry.register_::<u64, _>(|value: &mut u64| *value += 1);

    let record = registry
        .lookup_::<u64>()
        .expect("注册过的类型必须能查到");
    assert!(!record.is_noop_(), "带钩子的登记不应是空操作");
}

/// 验证注册表的键不会让不同类型的钩子互相串味（这是 D2c 的核心风险）。
/// - 手段：给 `A(u64)` 注册钩子；`B(u64)` 与它同尺寸、同对齐，且两者都不需要 `Drop`，
///   因此二者"无钩子记录"的内容完全相同、允许被编译器合并成同一份常量。
/// - 判断：`lookup_::<A>` 命中，而 `lookup_::<B>` 必须为 `None`。若拿记录地址当键，这一条
///   就会因为常量合并而失败；用 `typeid::of` 得到的类型 ID 作键则不会。
#[test]
fn pre_drop_registry_key_does_not_leak_across_types() {
    #[repr(transparent)]
    struct A(u64);
    #[repr(transparent)]
    struct B(u64);

    assert_eq!(
        core::mem::size_of::<A>(),
        core::mem::size_of::<B>(),
        "本测试要求两者同尺寸，才足以复现常量合并的风险"
    );

    let registry = PreDropRegistry::new();
    registry.register_::<A, _>(|value: &mut A| value.0 += 1);

    assert!(registry.lookup_::<A>().is_some(), "注册过的类型必须命中");
    assert!(
        registry.lookup_::<B>().is_none(),
        "不同类型的类型级钩子不能串味"
    );
    assert!(
        registry.lookup_::<String>().is_none(),
        "未注册的类型不能命中"
    );
}

/// 验证类型键支持**非 `'static`** 的类型参数——这正是引入 `typeid` 的理由。
/// - 手段：用一个带生命周期参数的类型 `Borrowed<'a>` 注册类型级钩子，再查找同一类型；
///   生命周期会被 `typeid::of` 规范化掉，因此注册与查找必须命中同一个键。
/// - 判断：`lookup_::<Borrowed<'_>>` 命中。若改用 `core::any::TypeId::of`，这段代码根本
///   无法通过编译（它要求 `T: 'static`）。
#[test]
fn pre_drop_registry_key_accepts_non_static_types() {
    struct Borrowed<'a>(core::marker::PhantomData<&'a u64>);

    fn hook_borrowed(_: &mut Borrowed<'_>) {}

    let registry = PreDropRegistry::new();
    registry.register_::<Borrowed<'_>, _>(hook_borrowed);
    assert!(
        registry.lookup_::<Borrowed<'_>>().is_some(),
        "非 'static 的类型也必须能登记与命中",
    );
}

/// 验证"对象级 > 类型级 > 默认"的优先级。
/// - 手段：给 `String` 注册类型级钩子；另造一个对象级登记；分别以不同组合调用 `resolve_`。
/// - 判断：给了对象级就取对象级；只给类型级就取类型级；都没有则退回
///   `PreDropRecord::of::<String>()`。
#[test]
fn object_level_record_overrides_type_level() {
    let registry = PreDropRegistry::new();
    registry.register_::<String, _>(|text: &mut String| text.clear());
    let type_level = registry
        .lookup_::<String>()
        .expect("类型级登记应当存在");
    let object_level = record_with_::<String, _>(|text: &mut String| text.push('!'));

    assert!(
        ptr::eq(resolve_::<String>(Some(&registry), None), type_level),
        "没有对象级钩子时应取类型级"
    );
    assert!(
        ptr::eq(
            resolve_::<String>(Some(&registry), Some(object_level)),
            object_level
        ),
        "对象级钩子必须覆盖类型级"
    );
    assert!(
        ptr::eq(
            resolve_::<String>(None, None),
            PreDropRecord::of::<String>()
        ),
        "两者都没有时应退回默认记录"
    );
}

/// 探针类型：记录自己被 `PreDrop` 看到的内容，并统计析构次数。
struct Probe(&'static str);

static PRE_DROPPED: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicUsize = AtomicUsize::new(0);

impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::AcqRel);
    }
}

/// 对象级钩子：必须能看到仍然有效的数据。
fn probe_hook(probe: &mut Probe) {
    assert_eq!(probe.0, "hello", "PreDrop 运行时数据必须仍然有效");
    PRE_DROPPED.fetch_add(1, Ordering::AcqRel);
}

/// 在栈上造一个真实身份槽位（`WeakChunk<()>`）与一块 `StrongChunk<Probe>`，返回强块首址。
///
/// # Safety
///
/// 返回值借用自 `slot`，调用方必须让 `slot` 活得比使用期更久。
unsafe fn make_probe_chunk_(slot: &mut MaybeUninit<StrongChunk<Probe>>) -> *mut u8 {
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    // SAFETY: weak_slot 可写；init_slots 会把它当作未初始化内存初始化
    let weak = unsafe { &mut *weak_slot.as_mut_ptr() };
    WeakChunk::<()>::init_slots(core::slice::from_mut(weak), 1);

    let chunk = slot.as_mut_ptr();
    // SAFETY: chunk 可写；weak 是本测试栈上的有效身份槽位
    unsafe {
        (*chunk).init_with_(core::ptr::NonNull::from(weak), Probe("hello"));
    }
    chunk.cast::<u8>()
}

/// 验证清理次序与次数：`PreDrop` 先跑、能看到有效数据，随后才发生 `Drop`。
/// - 手段：在栈上就地构造 `StrongChunk<Probe>`，用对象级记录 `run_` 触发一次清理。
/// - 判断：钩子计数与 `Drop` 计数都恰好为 1；钩子内部断言数据内容为 `"hello"`。
#[test]
fn pre_drop_runs_before_drop_exactly_once() {
    PRE_DROPPED.store(0, Ordering::Release);
    DROPPED.store(0, Ordering::Release);

    let mut slot = MaybeUninit::<StrongChunk<Probe>>::uninit();
    // SAFETY: slot 在本测试栈上，活得比 base 的使用期更久
    let base = unsafe { make_probe_chunk_(&mut slot) };
    let record = record_with_::<Probe, _>(probe_hook);

    // SAFETY: base 来自上面刚构造好的 StrongChunk<Probe>；Probe 是 Sized，元数据为空
    unsafe { record.run_(base, ptr::null()) };

    assert_eq!(PRE_DROPPED.load(Ordering::Acquire), 1, "PreDrop 应恰好运行一次");
    assert_eq!(DROPPED.load(Ordering::Acquire), 1, "Drop 应恰好运行一次");
}

/// 验证注册表的自旋锁真的把并发访问串行化了。
/// - 手段：把同一个注册表放进 `Arc`，4 个线程各重复 64 次"注册 + 查找"同一类型。
/// - 判断：所有线程都能查到登记，且全程没有数据竞争（若没有锁，`Vec` 的并发读写会破坏
///   内存或让查找失败）。这正是"不能拿 `&mut` 别名共享给多线程"那处隐患的回归测试。
#[test]
fn pre_drop_registry_is_thread_safe() {
    use std::sync::Arc;
    use std::thread;

    let registry = Arc::new(PreDropRegistry::new());
    let mut handles = alloc::vec::Vec::new();
    for _ in 0..4usize {
        let worker_registry = Arc::clone(&registry);
        handles.push(thread::spawn(move || {
            for _ in 0..64usize {
                worker_registry.register_::<u64, _>(|value: &mut u64| *value += 1);
                assert!(
                    worker_registry.lookup_::<u64>().is_some(),
                    "并发注册之后必须能查到"
                );
            }
        }));
    }
    for handle in handles {
        handle.join().expect("工作线程不应 panic");
    }
    assert!(registry.lookup_::<u64>().is_some());
}
