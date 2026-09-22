use core::sync::atomic::AtomicUsize;

use super::cell_::LocksOrderings;
use super::signal_::{LsbAsMutexSignal, MsbAsMutexSignal, TrMutexSignal};
use super::spin_::{SpinFlag, SpinMutex};

/// 测试默认 `SpinMutex`（MSB 锁信号）的加锁 / 解锁与受保护数据的读写。
/// - 手段：`SpinMutex::<usize>::new(0)`，在 `with_locked` 内断言已上锁并把值加一。
/// - 判断：进入前与退出后 `is_acquired` 为假、临界区内为真；最终值精确为 1。
#[test]
fn spin_mutex_acquires_and_releases_with_default_signal() {
    let mutex = SpinMutex::<usize>::new(0);
    assert!(!mutex.is_acquired());
    mutex.with_locked(|value| {
        assert!(mutex.is_acquired());
        *value += 1;
    });
    assert!(!mutex.is_acquired());
    assert_eq!(mutex.try_with_locked(0, |value| *value), Some(1));
}

/// 测试锁信号位与内存序确实是可配置项：换用最低位信号 + 宽松内存序后锁依旧可用。
/// - 手段：把 `S` 换成 `LsbAsMutexSignal<usize>`、`O` 换成 `LocksOrderings`，做一次加锁写入；
///   并直接对两种策略的判据分别断言。
/// - 判断：临界区内可写、退出后可读；`LsbAsMutexSignal` 只认最低位，`MsbAsMutexSignal` 只认最高位。
#[test]
fn spin_mutex_supports_configurable_signal_bit_and_orderings() {
    let mutex =
        SpinMutex::<usize, usize, AtomicUsize, LsbAsMutexSignal<usize>, LocksOrderings>::new(0);
    assert!(!mutex.is_acquired());
    mutex.with_locked(|value| *value += 41);
    assert!(!mutex.is_acquired());
    assert_eq!(mutex.try_with_locked(0, |value| *value), Some(41));

    assert!(LsbAsMutexSignal::<usize>::is_acquired(0b01));
    assert!(!LsbAsMutexSignal::<usize>::is_acquired(0b10));
    assert!(MsbAsMutexSignal::<usize>::is_acquired(1usize << (usize::BITS - 1)));
    assert!(!MsbAsMutexSignal::<usize>::is_acquired(0b01));
}

/// 测试 `SpinMutex` 跨线程互斥，受保护内容的更新不丢失。
/// - 手段：4 个线程各自在 `with_locked` 临界区内把计数加一，共 4 × 256 次。
/// - 判断：最终计数精确等于 1024；任何丢失更新都会让它小于 1024。
#[test]
fn spin_mutex_serializes_concurrent_updates() {
    let mutex = SpinMutex::<usize>::new(0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..256 {
                    mutex.with_locked(|value| *value += 1);
                }
            });
        }
    });
    assert_eq!(mutex.try_with_locked(0, |value| *value), Some(1024));
}

/// 测试临界区 panic 时锁一定会被守卫归还。
/// - 手段：在 `catch_unwind` 中进入 `with_locked` 后主动 panic，捕获后再进入一次临界区。
/// - 判断：panic 被捕获、`is_acquired` 回到假，且第二次进入仍能读到原值。
#[test]
fn spin_mutex_lock_is_released_when_closure_panics() {
    let mutex = SpinMutex::<usize>::new(7);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mutex.with_locked(|_| panic!("故意 panic 以验证守卫归还锁"));
    }));
    assert!(caught.is_err());
    assert!(!mutex.is_acquired());
    assert_eq!(mutex.try_with_locked(0, |value| *value), Some(7));
}

/// 测试 `SpinFlag`：`read` 不暴露锁位，且 `try_update` 把业务位的读—改—写串行化。
/// - 手段：先手动 `try_acquire` / `release` 观察锁位语义；再让 4 个线程各自 `try_update`
///   把业务位加一，共 4 × 256 次。
/// - 判断：持锁期间 `is_locked` 为真且 `read` 仍为 0（锁位不可见）、二次 `try_acquire` 失败；
///   并发更新后 `read` 精确等于 1024。
#[test]
fn spin_flag_hides_lock_bit_and_serializes_updates() {
    let flag = SpinFlag::<u32>::new(0);
    assert!(!flag.is_locked());
    assert!(flag.try_acquire(4));
    assert!(flag.is_locked());
    assert_eq!(flag.read(), 0);
    assert!(!flag.try_acquire(4));
    assert!(flag.release());
    assert!(!flag.is_locked());

    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..256 {
                    let _ = flag.try_update(|bits| Some((bits + 1, ())));
                }
            });
        }
    });
    assert_eq!(flag.read(), 1024);
}

/// 测试 `SpinFlag` 的业务位计数：`fetch_add` / `fetch_sub` 返回运算前的值。
/// - 手段：从 0 起 `fetch_add(1)`、`fetch_add(41)`，再 `fetch_sub(2)`。
/// - 判断：返回的旧值依次为 0、1、42，且 `read` 依次给出 42、40。
#[test]
fn spin_flag_counts_with_fetch_add_and_sub() {
    let flag = SpinFlag::<u32>::new(0);
    assert_eq!(flag.fetch_add(1), 0);
    assert_eq!(flag.fetch_add(41), 1);
    assert_eq!(flag.read(), 42);
    assert_eq!(flag.fetch_sub(2), 42);
    assert_eq!(flag.read(), 40);
}
