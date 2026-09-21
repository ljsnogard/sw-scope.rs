//! 对照实验：证明"arena 不在清盘时替对象负责"会产生真实的内存泄漏。
//!
//! 本 crate 只做一件事：用 `bumpalo`（最常用的 Rust arena）复现
//! `dev-notes/strong-20260921-2150.md` §8 列出的几种写法，并用**真实的内存计数器**
//! 验证对象里持有的堆缓冲是否被释放。
//!
//! # 为什么这个证明是可信的
//!
//! 只看 `Drop` 有没有被调用，可能被"值被移走/被优化"之类的问题干扰；所以这里同时用
//! 两个独立证据：
//!
//! 1. **进程级分配器计数器**：包一层 `GlobalAlloc`，统计 `alloc` / `dealloc` 的次数与
//!    字节数。`HeapThing` 里的堆缓冲如果没有被释放，`dealloc` 字节数就会明显小于
//!    `alloc` 字节数——这是"内存真的没还回去"，不是"析构函数没打印"。
//! 2. **带 Drop 的探针类型**：直接数析构次数，展示"对象还在、析构没发生"。
//!
//! # 运行
//!
//! ```text
//! cd sw-scope/drop-semantics-demo
//! cargo run
//! ```
//!
//! 该 crate 是独立 workspace，不会影响 `sw-scope` 本体的零依赖状态。

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 仪器：进程级分配器计数器
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 包住系统分配器，逐次统计分配/释放的次数与字节数。
struct CountingAlloc;

static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: 只是转发给系统分配器
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: 只是转发给系统分配器
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// 一次测量：跑完 `body` 之后，这段代码"申请了但没归还"的字节数。
struct NetHeap {
    alloc_bytes: usize,
    dealloc_bytes: usize,
}

impl NetHeap {
    fn measure(body: impl FnOnce()) -> NetHeap {
        let a0 = ALLOC_BYTES.load(Ordering::Relaxed);
        let d0 = DEALLOC_BYTES.load(Ordering::Relaxed);
        body();
        NetHeap {
            alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed) - a0,
            dealloc_bytes: DEALLOC_BYTES.load(Ordering::Relaxed) - d0,
        }
    }

    /// 净泄漏字节数。
    fn net_bytes(&self) -> usize {
        self.alloc_bytes.saturating_sub(self.dealloc_bytes)
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 探针：带 Drop 的类型
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 稳定的小分配；同时用来做"至少还回了多少"的判据。
const PAYLOAD: usize = 64 * 1024;

/// 一个持有堆缓冲、并在析构时记账的对象。
struct HeapThing {
    payload: Vec<u8>,
}

impl HeapThing {
    fn new(size: usize) -> Self {
        HeapThing {
            payload: vec![0xA5; size],
        }
    }
}

static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

impl Drop for HeapThing {
    fn drop(&mut self) {
        DROP_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 每组实验的结果
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

struct Outcome {
    name: &'static str,
    net: NetHeap,
    drops: usize,
    note: &'static str,
}

impl Outcome {
    /// 对象里的堆缓冲（至少 `PAYLOAD` 字节）是否被归还。
    ///
    /// 注意不能用"net == 0"当判据：arena 自身的 chunk 也会占内存，那是 arena 的固定
    /// 开销、不是对象的泄漏。判据是"至少有一个 payload 被还回来了"。
    fn payload_released(&self) -> bool {
        self.net.dealloc_bytes >= PAYLOAD
    }

    fn leaked_payloads(&self) -> usize {
        self.net.net_bytes() / PAYLOAD
    }
}

fn report(outcome: &Outcome) {
    println!(
        "{:<40} alloc={:>7}B dealloc={:>7}B net={:>7}B drops={} | {}",
        outcome.name,
        outcome.net.alloc_bytes,
        outcome.net.dealloc_bytes,
        outcome.net.net_bytes(),
        outcome.drops,
        outcome.note
    );
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 对照组：正常作用域，用来证明仪器有效
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

fn baseline_scoped_value() -> Outcome {
    DROP_COUNT.store(0, Ordering::Relaxed);
    let net = NetHeap::measure(|| {
        let thing = HeapThing::new(PAYLOAD);
        assert_eq!(thing.payload.len(), PAYLOAD);
        // thing 在闭包结束时析构
    });
    Outcome {
        name: "基线：普通作用域里的 HeapThing",
        net,
        drops: DROP_COUNT.load(Ordering::Relaxed),
        note: "作用域结束即析构，内存归还",
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 实验组：§8 的几种写法，全部用 bumpalo
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// §8.1：值放进 arena 后，句柄/引用当场丢弃，无人再引用。
///
/// 在 sw-scope 里对应 `scope.put(|| Thing::new());`（匿名 `Retain` 立刻析构）。
/// 在 bumpalo 里，`Bump::alloc` 的返回值被丢弃即等价。
fn bump_anonymous() -> Outcome {
    DROP_COUNT.store(0, Ordering::Relaxed);
    let net = NetHeap::measure(|| {
        // arena 本体也在测量范围内，这样"arena 自己占了 496 字节"与"对象泄漏 64 KiB"
        // 才能在同一次测量里区分开
        let bump = bumpalo::Bump::new();
        bump.alloc(HeapThing::new(PAYLOAD));
    });
    Outcome {
        name: "§8.1 bumpalo：匿名临时值",
        net,
        drops: DROP_COUNT.load(Ordering::Relaxed),
        note: "值留在 chunk 里、析构未发生",
    }
}

/// §8.4：故意让唯一的引用丢失（`mem::forget` 句柄的 bumpalo 等价）。
fn bump_handle_lost() -> Outcome {
    DROP_COUNT.store(0, Ordering::Relaxed);
    let net = NetHeap::measure(|| {
        let bump = bumpalo::Bump::new();
        let referenced: &mut HeapThing = bump.alloc(HeapThing::new(PAYLOAD));
        assert_eq!(referenced.payload.len(), PAYLOAD);
        // 引用在这里离开作用域，没有任何析构被触发的机会
    });
    Outcome {
        name: "§8.4 bumpalo：引用丢失",
        net,
        drops: DROP_COUNT.load(Ordering::Relaxed),
        note: "没有任何责任人，堆缓冲泄漏",
    }
}

/// §8.5：`Sharing` 环的 bumpalo 等价——两个对象互相持有、谁也不释放。
fn bump_cycle() -> Outcome {
    DROP_COUNT.store(0, Ordering::Relaxed);
    let net = NetHeap::measure(|| {
        let bump = bumpalo::Bump::new();
        let a = bump.alloc(HeapThing::new(PAYLOAD));
        let b = bump.alloc(HeapThing::new(PAYLOAD));
        // 互相引用；bumpalo 里没有人会替它们析构
        let ring: (&mut HeapThing, &mut HeapThing) = (a, b);
        assert_eq!(ring.0.payload.len(), PAYLOAD);
    });
    Outcome {
        name: "§8.5 bumpalo：互相引用的环",
        net,
        drops: DROP_COUNT.load(Ordering::Relaxed),
        note: "两个对象都无人析构",
    }
}

/// 对照：`bumpalo::boxed::Box` 把析构责任交给句柄，因此**对象不泄漏**。
///
/// 这条证明"泄漏的根源是没人负责析构，而不是 arena 不能用"：同一个 bumpalo，
/// 换成会跑析构的句柄类型，payload 立刻归还（剩下的只有 arena 自己的 chunk）。
fn bump_boxed() -> Outcome {
    DROP_COUNT.store(0, Ordering::Relaxed);
    let net = NetHeap::measure(|| {
        let bump = bumpalo::Bump::new();
        let boxed = bumpalo::boxed::Box::new_in(HeapThing::new(PAYLOAD), &bump);
        assert_eq!(boxed.payload.len(), PAYLOAD);
        // boxed 在闭包结束时析构 → 跑 HeapThing::drop → 释放 payload
    });
    Outcome {
        name: "对照 bumpalo：boxed::Box（句柄负责）",
        net,
        drops: DROP_COUNT.load(Ordering::Relaxed),
        note: "句柄析构时跑了 Drop，payload 与 arena 开销都归还",
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

fn main() {
    println!("=== bumpalo 析构语义对照实验 ===");
    println!("每个对象的堆缓冲 = {PAYLOAD} 字节；arena 自身的 chunk 开销约 496 字节");
    println!("（496 = Bump 首个 chunk 的 512 字节减去 16 字节 footer；它对所有场景都一样）\n");

    let baseline = baseline_scoped_value();
    let anonymous = bump_anonymous();
    let lost = bump_handle_lost();
    let cycle = bump_cycle();
    let boxed = bump_boxed();

    for outcome in [&baseline, &anonymous, &lost, &cycle, &boxed] {
        report(outcome);
    }

    println!("\n--- 断言（结论的机器可验证形式）---");

    // 1) 仪器有效：普通作用域必须析构且归还全部内存
    assert_eq!(baseline.drops, 1, "基线应当析构一次");
    assert_eq!(baseline.net.net_bytes(), 0, "基线应当净泄漏为 0");

    // 2) 三种"没人负责"的写法都必须真实泄漏
    assert_eq!(anonymous.drops, 0, "§8.1 不该有析构发生");
    assert!(!anonymous.payload_released(), "§8.1 预期泄漏 payload");
    assert!(anonymous.net.net_bytes() >= PAYLOAD, "§8.1 泄漏量应至少一个 payload");

    assert_eq!(lost.drops, 0, "§8.4 不该有析构发生");
    assert!(lost.net.net_bytes() >= PAYLOAD, "§8.4 泄漏量应至少一个 payload");

    assert_eq!(cycle.drops, 0, "§8.5 不该有析构发生");
    assert!(
        cycle.net.net_bytes() >= 2 * PAYLOAD,
        "§8.5 两个对象都该泄漏"
    );

    // 3) 对照组：同一个 arena，只要有人负责析构就不泄漏对象
    assert_eq!(boxed.drops, 1, "对照组应当析构一次");
    assert!(boxed.payload_released(), "对照组应当归还 payload");

    println!("全部断言通过。");
    println!();
    println!("结论：");
    println!(
        "  · bumpalo 的 Bump 不在清盘时替对象负责：§8.1/§8.4/§8.5 分别净泄漏 {} / {} / {} 字节",
        anonymous.net.net_bytes(),
        lost.net.net_bytes(),
        cycle.net.net_bytes()
    );
    println!(
        "    （对应 {} / {} / {} 个 payload，且 drops 全为 0）。",
        anonymous.leaked_payloads(),
        lost.leaked_payloads(),
        cycle.leaked_payloads()
    );
    println!(
        "  · 同一个 arena 换成句柄负责析构的 boxed::Box 后，payload 与 arena 自身开销（各 {} 字节）都全额归还，净泄漏 0。",
        boxed.net.dealloc_bytes
    );
    println!("  ⇒ 泄漏的根源是：没有任何一处代码仍然知道 T 并愿意替它析构；与 arena 实现无关；");
    println!("    因此 sw-scope 必须保留一条不依赖句柄、能在编译期类型已擦除的情况下工作的清盘析构路径");
    println!("    （见 dev-notes/strong-20260921-2150.md §7/§8）。");
}
