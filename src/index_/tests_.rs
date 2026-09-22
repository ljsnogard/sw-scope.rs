//! `Owning` / `Sharing` / `Retain` 的引用计数与"确定性析构"语义测试。
//!
//! 对应 `dev-notes/weak-20260922-1135.md` §2、§6：`Owning` / `Sharing` 析构只归还访问权，
//! 只有"最后一个 `Retain` 也消失、且没有强引用残留"才走确定性析构（来源 (a)）。

use core::{mem::MaybeUninit, ptr::NonNull, sync::atomic::{AtomicUsize, Ordering}};

use super::{Owning, Retain, Sharing};
use crate::strong_::StrongChunk;
use crate::weak_::{DataState, WeakChunk};

/// 在栈上造一个真实的"身份槽位 + 强块"，并包出一个弱计数为 1 的 `Retain<T>`。
///
/// # Safety
///
/// 两个 `MaybeUninit` 槽位必须在返回的 `Retain` 使用期间一直存活；本函数会就地初始化它们。
unsafe fn make_retain_<T>(
    weak_slot: &mut MaybeUninit<WeakChunk<()>>,
    chunk_slot: &mut MaybeUninit<StrongChunk<T>>,
    value: T,
) -> Retain<T> {
    // SAFETY: 由调用方保证两个槽位可写且存活足够久
    let weak = unsafe { &mut *weak_slot.as_mut_ptr() };
    WeakChunk::<()>::init_slots(core::slice::from_mut(weak), 1);
    let weak_ptr = NonNull::from(&mut *weak);
    // SAFETY: chunk_slot 可写；weak 是本测试栈上的有效身份槽位
    unsafe { (*chunk_slot.as_mut_ptr()).init_with_(weak_ptr, value) };
    weak.chunk_state().init_created();
    // 一个 Retain 对应一个弱计数
    weak.incr_weak_count();
    Retain::new(weak_ptr.cast())
}

/// 验证 `Owning` 析构只归还访问权：数据仍然活着、可以再次独占。
/// - 手段：造一个 `Retain<u64>`，取出 `Owning` 后立刻析构，再读状态并再次升级。
/// - 判断：状态必须回到 `Created`；第二次 `try_owning` 必须成功且读到原值。
#[test]
fn owning_drop_returns_to_created_and_data_stays_alive() {
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    let mut chunk_slot = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: 两个槽位都在本测试栈上，活得比 retain 更久
    let retain = unsafe { make_retain_(&mut weak_slot, &mut chunk_slot, 42u64) };

    {
        let owning = retain.try_owning().expect("Created 应当能升级为 Owning");
        assert_eq!(*owning, 42);
    }

    let weak = unsafe { &*weak_slot.as_ptr() };
    assert_eq!(
        weak.data_state(),
        DataState::Created,
        "Owning 析构后应回到 Created"
    );
    let owning = retain.try_owning().expect("数据仍活着，应当能再次独占");
    assert_eq!(*owning, 42);
}

/// 验证 `Sharing` 的强计数增减与"归零回 `Created`"。
/// - 手段：升级为 `Sharing`、克隆一次、逐个析构，每步读强计数与状态。
/// - 判断：计数依次为 1、2、1、0；只剩一个共享者时状态保持 `Sharing`（不能回到 `Created`），
///   计数归零后才回到 `Created`。
#[test]
fn sharing_counts_and_returns_to_created() {
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    let mut chunk_slot = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: 两个槽位都在本测试栈上
    let retain = unsafe { make_retain_(&mut weak_slot, &mut chunk_slot, 7u64) };
    let chunk = unsafe { &*chunk_slot.as_ptr() };
    let weak = unsafe { &*weak_slot.as_ptr() };

    let sharing = retain.try_sharing().expect("Created 应能升级为 Sharing");
    assert_eq!(chunk.strong_count(), 1, "首次共享后强计数为 1");
    let sharing2 = sharing.clone();
    assert_eq!(chunk.strong_count(), 2, "克隆应当加计数");

    drop(sharing);
    assert_eq!(chunk.strong_count(), 1);
    assert_eq!(
        weak.data_state(),
        DataState::Sharing,
        "还有共享者时不能回到 Created"
    );

    drop(sharing2);
    assert_eq!(chunk.strong_count(), 0);
    assert_eq!(
        weak.data_state(),
        DataState::Created,
        "强计数归零后回到 Created"
    );
}

/// 验证 `try_sharing` 的幂等增量：已经是 `Sharing` 时仍能成功并加计数。
/// - 手段：连续两次 `try_sharing`。
/// - 判断：第二次必须返回 `Some`，且强计数变成 2。这是 README 里"泄漏一个 `Sharing`
///   之后仍能继续共享"的最小复现。
#[test]
fn try_sharing_is_idempotent_increment() {
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    let mut chunk_slot = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: 两个槽位都在本测试栈上
    let retain = unsafe { make_retain_(&mut weak_slot, &mut chunk_slot, 5u64) };
    let chunk = unsafe { &*chunk_slot.as_ptr() };

    let first = retain.try_sharing().expect("首次共享应当成功");
    let second = retain
        .try_sharing()
        .expect("已经是 Sharing 时仍应成功（幂等增量）");
    assert_eq!(chunk.strong_count(), 2, "第二次共享应当只是加计数");
    drop(first);
    drop(second);
}

/// 只在最后一个句柄消失时析构一次的探针（独立静态，避免测试并行互相干扰）。
struct LastProbe;
static LAST_DROPPED: AtomicUsize = AtomicUsize::new(0);
impl Drop for LastProbe {
    fn drop(&mut self) {
        LAST_DROPPED.fetch_add(1, Ordering::AcqRel);
    }
}

/// 只在最后一个句柄消失时析构一次的探针（另一个独立静态）。
struct CloneProbe;
static CLONE_DROPPED: AtomicUsize = AtomicUsize::new(0);
impl Drop for CloneProbe {
    fn drop(&mut self) {
        CLONE_DROPPED.fetch_add(1, Ordering::AcqRel);
    }
}

/// 验证"最后一个 `Retain` 消失即确定性析构"。
/// - 手段：造一个 `Retain<LastProbe>` 后直接析构它。
/// - 判断：`LastProbe` 的析构计数必须从 0 变成 1，且槽位状态进入 `Destroyed`。
#[test]
fn last_retain_drop_destroys_immediately() {
    LAST_DROPPED.store(0, Ordering::Release);
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    let mut chunk_slot = MaybeUninit::<StrongChunk<LastProbe>>::uninit();
    // SAFETY: 两个槽位都在本测试栈上
    let retain = unsafe { make_retain_(&mut weak_slot, &mut chunk_slot, LastProbe) };

    assert_eq!(LAST_DROPPED.load(Ordering::Acquire), 0);
    drop(retain);
    assert_eq!(
        LAST_DROPPED.load(Ordering::Acquire),
        1,
        "最后一个 Retain 析构应当立即析构数据"
    );
    let weak = unsafe { &*weak_slot.as_ptr() };
    assert_eq!(weak.data_state(), DataState::Destroyed);
}

/// 验证存在多个句柄时不会提前析构。
/// - 手段：克隆一个 `Retain<CloneProbe>`，先析构其中一个，再析构另一个。
/// - 判断：第一次析构后计数仍为 0；第二次析构后计数为 1。
#[test]
fn data_survives_until_last_cloned_retain_is_dropped() {
    CLONE_DROPPED.store(0, Ordering::Release);
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    let mut chunk_slot = MaybeUninit::<StrongChunk<CloneProbe>>::uninit();
    // SAFETY: 两个槽位都在本测试栈上
    let retain = unsafe { make_retain_(&mut weak_slot, &mut chunk_slot, CloneProbe) };
    let retain2 = retain.clone();

    drop(retain);
    assert_eq!(
        CLONE_DROPPED.load(Ordering::Acquire),
        0,
        "还有句柄存活时不能析构"
    );
    drop(retain2);
    assert_eq!(
        CLONE_DROPPED.load(Ordering::Acquire),
        1,
        "最后一个句柄消失才析构"
    );
}

/// 验证三种句柄的 `Send` / `Sync` marker 采用与 `Box` / `Arc` 一致的边界。
///
/// - 手段：对 `T = u64`（`Send + Sync`）做静态 trait 约束断言，不实际跨线程发送。
/// - 判断：所有断言必须编译通过；若 marker 边界写错，`Owning` / `Sharing` / `Retain`
///   将无法满足 `Send` / `Sync`，测试编译失败。
#[test]
fn handle_markers_follow_box_and_arc_bounds() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    // `Owning` 比照 `Box<T>`：Send 看 T: Send，Sync 看 T: Sync。
    assert_send::<Owning<'static, u64>>();
    assert_sync::<Owning<'static, u64>>();

    // `Sharing` 与 `Retain` 比照 `Arc<T>`：二者都要求 T: Send + Sync。
    assert_send::<Sharing<'static, u64>>();
    assert_sync::<Sharing<'static, u64>>();
    assert_send::<Retain<u64>>();
    assert_sync::<Retain<u64>>();
}

/// 验证 `Retain` 作为 `Sync` 句柄跨线程共享时，状态机不会把 `Created` / `Owning` /
/// `Sharing` 迁移撕开。
///
/// - 手段：在 `std::thread::scope` 中让 4 个线程共享同一个 `Retain<u64>`，反复尝试
///   `try_owning`（成功则改值）或 `try_sharing`（成功则克隆、读取再析构）。
/// - 判断：所有线程正常结束；最终值必须等于所有线程成功独占并写入的次数。若状态锁缺失，
///   可能出现 `Owning` 已生效却仍有 `Sharing` 持有着、或强计数回退时被重新加回等撕裂。
#[test]
fn retain_state_machine_is_safe_across_threads() {
    let mut weak_slot = MaybeUninit::<WeakChunk<()>>::uninit();
    let mut chunk_slot = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: 两个栈槽位活得比 `retain` 和 scoped threads 更久，线程会在函数返回前 join。
    let retain = unsafe { make_retain_(&mut weak_slot, &mut chunk_slot, 0u64) };
    let owning_wins = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..500 {
                    if let Some(mut owning) = retain.try_owning() {
                        *owning += 1;
                        owning_wins.fetch_add(1, Ordering::AcqRel);
                        drop(owning);
                    } else if let Some(sharing) = retain.try_sharing() {
                        let clone = sharing.clone();
                        let _ = *clone;
                        drop(clone);
                        drop(sharing);
                    }
                }
            });
        }
    });

    assert_eq!(
        *retain.try_owning().expect("线程结束后应回到 Created"),
        owning_wins.load(Ordering::Acquire) as u64,
        "最终值必须等于成功独占写入的次数"
    );
}
