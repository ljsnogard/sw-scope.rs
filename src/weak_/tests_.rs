//! 内部单元测试。
//!
//! 这里的测试对象都是 crate 私有项（`WeakChunk` / `WeakChunkPool` 等），
//! 无法从 `tests/` 目录下的集成测试访问，因此集中放在本模块内。
//! `demo_.rs` 只作为使用示例，不承担测试职责。

use alloc::{alloc::Global, vec, vec::Vec};
use core::ptr::NonNull;

use crate::scope_inner_::{PoolIndex, ScopeInner, WeakChunkPool};
use crate::weak_::WeakChunk;

/// 验证 `WeakChunkPool` 布局所依赖的类型前提：`WeakChunk<T>` 的尺寸与对齐
/// 与 `T` 无关，且恰好等于 `WeakChunk<()>`。
/// - 手段：分别取 `T = ()`、`u8`、`u128`、`[u8]` 时 `WeakChunk<T>` 的 `size_of`
///   与 `align_of`，要求全部相等。
/// - 判断：任一尺寸或对齐不一致即断言失败；通过则说明池可以统一按
///   `WeakChunk<()>` 步长定位槽位，且 `chunks()` 返回的切片长度与池的实际容量一致。
#[test]
fn weak_chunk_size_is_type_agnostic() {
    use core::mem::{align_of, size_of};

    let base_size = size_of::<WeakChunk<()>>();
    let base_align = align_of::<WeakChunk<()>>();

    assert_eq!(base_size, size_of::<WeakChunk<u8>>());
    assert_eq!(base_size, size_of::<WeakChunk<u128>>());
    assert_eq!(base_size, size_of::<WeakChunk<[u8]>>());

    assert_eq!(base_align, align_of::<WeakChunk<u8>>());
    assert_eq!(base_align, align_of::<WeakChunk<u128>>());
    assert_eq!(base_align, align_of::<WeakChunk<[u8]>>());
}

/// 验证 `WeakChunkPool` 的容量换算与其自述的"池头 = 2 个槽位"布局一致。
/// - 手段：比较 `min_size_for_max_count` 与 `max_count_within_max_size` 的往返结果，
///   显式要求池头大小恰好等于 2 个 `WeakChunk<()>`；预算的边界取
///   "刚好放下 count 个槽位"与"少一个字节"两种。
/// - 判断：容量到尺寸的换算必须恰好加上池头大小，且少一个字节就应少放一个槽位；
///   池头若不是 2 个槽位，则池头按槽位大小整除定位的前提被打破，断言失败。
#[test]
fn weak_chunk_pool_capacity_math() {
    use core::mem::size_of;

    type Pool = WeakChunkPool<8>;

    let slot = size_of::<WeakChunk<()>>();
    // 池头必须恰好占 2 个槽位，cell_offset_ 的整除计算才成立
    assert_eq!(size_of::<Pool>(), 2 * slot);

    for count in [1u16, 7, 1000, PoolIndex::MAX] {
        let need = Pool::min_size_for_max_count(count);
        assert_eq!(need, size_of::<Pool>() + count as usize * slot);
        // 恰好足够的预算应换回同样的槽位数
        assert_eq!(Pool::max_count_within_max_size(need), count as usize);
        // 少一个字节就再也放不下这么多槽位
        assert_eq!(Pool::max_count_within_max_size(need - 1), count as usize - 1);
    }

    // 预算连池头都放不下时没有任何槽位可用
    assert_eq!(Pool::max_count_within_max_size(0), 0);
    assert_eq!(Pool::max_count_within_max_size(size_of::<Pool>()), 0);
    assert_eq!(Pool::max_count_within_max_size(size_of::<Pool>() - 1), 0);
    // 刚好放下一个槽位的预算
    assert_eq!(Pool::max_count_within_max_size(size_of::<Pool>() + slot), 1);
}

/// 验证池初始化后空闲链表的初始形态：槽位 `i` 的 `next_freed_` 指向 `i + 1`，
/// 末槽指向 `capacity_`，表头 `latest_free_` 指向槽位 0；同时验证全部槽位都还处于
/// "序号未初始化"状态，说明链表的搭建没有顺手改动序号。
/// - 手段：用容量 4 构造池，经 `chunks()` 逐个读取槽位的 `pool_order` 与 `next_freed`。
/// - 判断：断言各槽位的链接值与 `i + 1` 一致、末槽为容量值、`pool_order` 等于数组下标；
///   若初始化未串成顺序链，后续 `allocate` 将无法从表头依次摘出全部槽位。
#[test]
fn weak_chunk_pool_init_builds_sequential_free_list() {
    let mut pool = WeakChunkPool::<8>::try_new_with_cell_count(4, &Global).unwrap();
    // SAFETY: pool 由 try_new_with_cell_count 分配，非空且在测试期间一直存活
    let pool = unsafe { pool.as_mut() };
    assert_eq!(pool.capacity(), 4);
    assert_eq!(pool.used_count(), 0);
    assert_eq!(pool.latest_free(), 0);

    // SAFETY: chunks() 返回的切片长度即 capacity_，索引 0..4 均在范围内
    let slots = unsafe { pool.chunks().as_mut() };
    assert_eq!(slots.len(), 4);
    for (i, slot) in slots.iter().enumerate() {
        assert_eq!(slot.next_freed(), (i + 1) as u16);
        // 序号在构造期就按数组下标写死，与分配与否无关
        assert_eq!(slot.pool_order(), i as u16);
        assert_eq!(slot.weak_count(), 0);
    }
}

/// 验证池未耗尽时的分配语义：`allocate` 从空闲链表表头摘下槽位并递增 `used_length_`，
/// 池满后返回 `None` 且不再改动状态；同时验证分配的返回序号与槽位自身记录的序号一致。
/// - 手段：用容量 3 构造池，连续调用 4 次 `allocate`，逐次读取槽位的 `pool_order`；
///   再归还一个槽位使池不再满，重新调用 `allocate`。
/// - 判断：前 3 次必须依次返回 0、1、2，第 4 次必须是 `None`；每个被分配槽位的
///   `pool_order` 必须等于其序号；`used_length_` 与 `free_length_` 必须始终互补地跟着
///   分配/归还变化；归还后必须还能再分配成功。
#[test]
fn weak_chunk_pool_allocate_pops_free_list_head() {
    let mut pool = WeakChunkPool::<8>::try_new_with_cell_count(3, &Global).unwrap();
    // SAFETY: pool 由 try_new_with_cell_count 分配，非空且在测试期间一直存活
    let pool = unsafe { pool.as_mut() };

    for expect in 0..3u16 {
        let index = pool.allocate().expect("池未满时必定能分配");
        assert_eq!(index, expect);
        // SAFETY: index 是刚由本池分配的槽位序号，必定小于 capacity_
        let slots = unsafe { pool.chunks().as_mut() };
        assert_eq!(slots[index as usize].pool_order(), index);
    }
    assert_eq!(pool.used_count(), 3);
    assert_eq!(pool.free_count(), 0);
    assert_eq!(pool.allocate(), None);
    assert_eq!(pool.used_count(), 3);
    assert_eq!(pool.free_count(), 0);

    assert!(pool.deallocate(1));
    assert_eq!(pool.used_count(), 2);
    assert_eq!(pool.free_count(), 1);
    assert_eq!(pool.allocate(), Some(1));
    assert_eq!(pool.used_count(), 3);
    assert_eq!(pool.free_count(), 0);
    assert_eq!(pool.allocate(), None);
}

/// 验证"池尚未耗尽就归还"的核心场景：归还的槽位成为空闲链表的表头，
/// 按后进先出（LIFO）顺序先于未被分配过的槽位被复用。
/// - 手段：容量 5 的池先分配 0、1、2，随后依次归还 1 与 0，再从链表上取到池满，
///   记录每一次 `allocate` 的返回值。
/// - 判断：归还后应依次摘出 0、1（先归还的 1 被压在 0 之下），之后才轮到从未分配过的
///   3、4，最后返回 `None`；若归还没有把 `latest_free_` 挪到刚归还的槽位，
///   这里会摘到越界序号或提前取到 3。
#[test]
fn weak_chunk_pool_reuses_returned_slots_before_virgin_ones() {
    let mut pool = WeakChunkPool::<8>::try_new_with_cell_count(5, &Global).unwrap();
    // SAFETY: pool 由 try_new_with_cell_count 分配，非空且在测试期间一直存活
    let pool = unsafe { pool.as_mut() };

    assert_eq!(pool.allocate(), Some(0));
    assert_eq!(pool.allocate(), Some(1));
    assert_eq!(pool.allocate(), Some(2));
    assert_eq!(pool.used_count(), 3);

    assert!(pool.deallocate(1));
    assert!(pool.deallocate(0));
    assert_eq!(pool.used_count(), 1);

    assert_eq!(pool.allocate(), Some(0));
    assert_eq!(pool.allocate(), Some(1));
    assert_eq!(pool.allocate(), Some(3));
    assert_eq!(pool.allocate(), Some(4));
    assert_eq!(pool.allocate(), None);
    assert_eq!(pool.used_count(), 5);
}

/// 验证归还接口的边界处理：越界序号与空池归还都必须被拒绝且不改变池状态，
/// 合法归还则清掉槽位上残留的弱引用计数。
/// - 手段：容量 2 的池先对越界序号 2 与空池的序号 0 调用 `deallocate`；
///   随后正常分配一个槽位、把它的弱计数加到 2 再归还。
/// - 判断：非法归还必须返回 `false` 且 `used_length_` 与 `latest_free_` 保持不变；
///   合法归还返回 `true`，被归还槽位的 `weak_count()` 归零，且再次分配会取回同一序号。
#[test]
fn weak_chunk_pool_deallocate_rejects_invalid_input() {
    let mut pool = WeakChunkPool::<8>::try_new_with_cell_count(2, &Global).unwrap();
    // SAFETY: pool 由 try_new_with_cell_count 分配，非空且在测试期间一直存活
    let pool = unsafe { pool.as_mut() };

    assert!(!pool.deallocate(2));
    assert!(!pool.deallocate(0));
    assert_eq!(pool.used_count(), 0);
    assert_eq!(pool.latest_free(), 0);

    let index = pool.allocate().unwrap();
    assert_eq!(index, 0);
    {
        // SAFETY: index 是刚由本池分配的槽位序号，必定小于 capacity_
        let slots = unsafe { pool.chunks().as_mut() };
        let chunk = &slots[index as usize];
        chunk.incr_use_count();
        chunk.incr_use_count();
        assert_eq!(chunk.weak_count(), 2);
    }
    assert!(pool.deallocate(index));
    assert_eq!(pool.used_count(), 0);
    // SAFETY: 与上段相同，index 在范围内
    let slots = unsafe { pool.chunks().as_mut() };
    assert_eq!(slots[index as usize].weak_count(), 0);
    assert_eq!(pool.allocate(), Some(index));
}

/// 验证 `try_new_with_max_size` 按预算取尽可能多的槽位，预算不足时以错误收场。
/// - 手段：以 `min_size_for_max_count(8)` 恰好的预算构造池并检查容量；
///   再用 0 字节预算与"恰好差一个字节够放 1 个槽位"的预算分别构造池。
/// - 判断：恰好预算必须得到 8 个槽位且能连续分配 8 次、第 9 次失败；
///   两种不足的预算都必须返回 `Err`。
#[test]
fn weak_chunk_pool_with_max_size_budget() {
    let mut pool = WeakChunkPool::<8>::try_new_with_max_size(
        WeakChunkPool::<8>::min_size_for_max_count(8),
        &Global,
    )
    .unwrap();
    // SAFETY: pool 由 try_new_with_max_size 分配，非空且在测试期间一直存活
    let pool = unsafe { pool.as_mut() };
    assert_eq!(pool.capacity(), 8);
    for expect in 0..8u16 {
        assert_eq!(pool.allocate(), Some(expect));
    }
    assert_eq!(pool.allocate(), None);

    assert!(WeakChunkPool::<8>::try_new_with_max_size(0, &Global).is_err());
    assert!(
        WeakChunkPool::<8>::try_new_with_max_size(
            WeakChunkPool::<8>::min_size_for_max_count(1) - 1,
            &Global,
        )
        .is_err()
    );
}

/// 验证槽位序号在池构造期就已按数组下标写死，并且此后终生不变：
/// 分配、归还、再分配都不会改写它。这是 `allocate` 无须付出任何序号写入代价的前提，
/// 也是"由槽位反推池地址"始终成立的基础。
/// - 手段：容量 3 的池在构造后立刻记录全部槽位的 `pool_order`；接着完整走一遍
///   分配 0、1、2 → 归还 1、0 → 再次分配 0、1、3 的流程。
/// - 判断：每个槽位的 `pool_order` 必须恒等于它在数组中的下标——构造后如此，
///   被分配后如此，归还后依然如此；一旦某一步把它改成了别的值（例如复用槽位时
///   改写序号），断言失败。
#[test]
fn weak_chunk_pool_slot_order_is_fixed_at_construction() {
    let mut pool = WeakChunkPool::<8>::try_new_with_cell_count(3, &Global).unwrap();
    // SAFETY: pool 由 try_new_with_cell_count 分配，非空且在测试期间一直存活
    let pool = unsafe { pool.as_mut() };

    // SAFETY: chunks() 返回的切片长度即 capacity_，索引 0..3 均在范围内
    let slots = unsafe { pool.chunks().as_mut() };
    let expected: [u16; 3] = [0, 1, 2];
    for (i, slot) in slots.iter().enumerate() {
        assert_eq!(slot.pool_order(), expected[i]);
    }

    assert_eq!(pool.allocate(), Some(0));
    assert_eq!(pool.allocate(), Some(1));
    assert_eq!(pool.allocate(), Some(2));
    assert!(pool.deallocate(1));
    assert!(pool.deallocate(0));
    assert_eq!(pool.allocate(), Some(0));
    assert_eq!(pool.allocate(), Some(1));

    // 完整走过一轮分配与归还之后，所有槽位的序号仍是构造时的那一份
    // SAFETY: 与上段相同，索引 0..3 均在范围内
    let slots = unsafe { pool.chunks().as_mut() };
    for (i, slot) in slots.iter().enumerate() {
        assert_eq!(slot.pool_order(), expected[i]);
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 池链的随机分配/归还正确性
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 本次随机测试所使用的池类型。容量只有 16，因此一个 u64 就足以记录占用位图。
type TestPool = WeakChunkPool<8>;

/// 本次随机测试中每个池的槽位容量。
const TEST_CAP: u16 = 16;

/// 池链的随机操作驱动：一个小型确定性伪随机流。
///
/// 测试要求"随机行为是一系列预先构造好的数字"，因此这里不用系统熵源：给定同一个种子，
/// 每次运行都会得到完全相同的操作序列，失败可复现。生成算法只做位混合与乘法，不引入
/// 任何外部依赖，也便于日后放大规模做性能测试。
struct OpStream {
    state_: u64,
}

impl OpStream {
    /// 以 `seed` 为起点构造。种子为 0 时会被替换成一个非零常量，避免退化。
    const fn new_(seed: u64) -> Self {
        let state_ = if seed == 0 { 0x2545_F491_4F6C_DD1D } else { seed };
        OpStream { state_ }
    }

    /// 取出下一个 64 位随机数。
    fn next_u64_(&mut self) -> u64 {
        self.state_ = self.state_.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state_;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// 取出 `len` 个随机数，作为"预先构造好的行为序列"。
    fn take_(&mut self, len: usize) -> Vec<u64> {
        let mut v = Vec::with_capacity(len);
        for _ in 0..len {
            v.push(self.next_u64_());
        }
        v
    }
}

/// 按容量 16 构造一个池，并交出所有权。
fn new_pool_() -> NonNull<TestPool> {
    TestPool::try_new_with_cell_count(TEST_CAP, &Global).expect("测试用池应当构造成功")
}

/// 以轮转方式跨越池链进行分配/归还，并在每一步之后校验所有池的内部数据依然有效。
///
/// # 测试目标
///
/// 验证 `WeakChunkPool` 串成双向链之后，经历随机的分配与归还，每个池的空闲链表都仍然
/// 正确：从表头出发能够走遍池中**全部**空闲槽位，不丢、不重、不越界、不成环，且与外部
/// 账本记录的占用情况逐位吻合。
///
/// # 测试手段
///
/// - 构造 `POOL_NUM` 个容量 16 的池，用 `link_siblings` 串成链表；
/// - 每个池先各分配一半槽位作为起点，占用情况记在外部账本 `allocated` 中；
/// - 用 `OpStream` 预先构造 `op` 数组，每个 u64 编码一次行为：最高位决定"归还"还是
///   "分配"，其余位提供随机起点。分配按轮转顺序在池链上推进，归还则从账本里随机挑一个
///   已占用槽位；
/// - 每一步操作之后都调用 `check_pool_invariants_` 校验一次；全部操作结束后再追加一次
///   "放空"检查：轮流把所有池从半满分配至全满，验证空闲链表确实能交出每一个空闲槽位。
///
/// # 判定标准
///
/// 任何一步出现下述情况即断言失败：空闲链表成环或越界；遍历得到的空闲槽位集合与账本
/// 不符；`used_count` / `free_count` 与账本不符；空闲槽位的 `pool_order` 不等于池内
/// 下标；分配返回的槽位在账本中已占用；归还的槽位在账本中未占用；池链出现断链、回指
/// 不对称或环。全部通过则说明链上所有池的分配算法在随机压力下依然自洽。
#[test]
fn weak_chunk_pool_chain_survives_random_alloc_and_free() {
    const POOL_NUM: usize = 4;
    const OPS: usize = 4096;
    const SEED: u64 = 0xC0FF_EE00_1234_5678;
    let cap = TEST_CAP as usize;
    assert!(cap <= 64, "容量超过 64 就无法用一个 u64 记账");

    // 1) 先把池链搭起来，并给每个池绑定同一个（仅用于测试的）域。
    // 测试只关心域指针能否原样存取，不需要真的构造一个 ScopeInner，因此这里用一个
    // 对齐的占位地址；真正解引用域指针的代码尚未在测试路径上。
    let mut anchor = 0u8;
    let root: NonNull<ScopeInner<8>> = NonNull::from(&mut anchor).cast();
    let mut owners: Vec<NonNull<TestPool>> = Vec::with_capacity(POOL_NUM);
    for _ in 0..POOL_NUM {
        owners.push(new_pool_());
    }
    for owner in owners.iter_mut() {
        // SAFETY: owner 指向独占分配的池
        unsafe { owner.as_mut() }.set_root(root);
    }
    for i in 0..POOL_NUM - 1 {
        // SAFETY: 这些池由 new_pool_ 独占分配，测试结束前都存活
        let (a, b) = unsafe {
            (
                owners[i].as_mut() as *mut TestPool,
                owners[i + 1].as_mut() as *mut TestPool,
            )
        };
        // SAFETY: a 与 b 指向不同的池
        unsafe { (*a).link_siblings(&mut *b) };
    }

    // 2) 起点：每个池都半满，并把占用情况写进外部账本
    let mut allocated: Vec<u64> = vec![0u64; POOL_NUM];
    for (p, owner) in owners.iter_mut().enumerate() {
        // SAFETY: owner 指向独占分配的池
        let pool = unsafe { owner.as_mut() };
        for slot in 0..(cap / 2) {
            assert_eq!(pool.allocate(), Some(slot as PoolIndex));
            allocated[p] |= 1u64 << slot;
        }
        assert_eq!(pool.used_count() as usize, cap / 2);
    }

    let mut stream = OpStream::new_(SEED);
    let ops = stream.take_(OPS);
    let mut cursor = 0usize;
    let mut alloc_ops = 0usize;
    let mut free_ops = 0usize;
    let mut skipped = 0usize;
    let mut last_alloc = Option::None;
    let mut last_free = Option::None;

    for (step, op) in ops.iter().enumerate() {
        let want_free = op & (1u64 << 63) != 0;
        let selector = (*op & 0xFFFF_FFFF) as usize;

        if want_free {
            // 从账本中随机挑一个已占用的槽位归还；若此刻没有任何占用则跳过这一步
            let occupied: usize = allocated
                .iter()
                .map(|bits| bits.count_ones() as usize)
                .sum();
            if occupied > 0 {
                let mut pick = selector % occupied;
                let mut target = Option::None;
                for (p, bits) in allocated.iter().enumerate() {
                    let n = bits.count_ones() as usize;
                    if pick < n {
                        let mut seen = 0usize;
                        for s in 0..cap {
                            if bits & (1u64 << s) != 0 {
                                if seen == pick {
                                    target = Option::Some((p, s));
                                    break;
                                }
                                seen += 1;
                            }
                        }
                        break;
                    }
                    pick -= n;
                }
                let (p, s) = target.expect("账本与选择逻辑应当一致");
                // SAFETY: owners[p] 指向独占分配的池
                let pool = unsafe { owners[p].as_mut() };
                assert!(
                    pool.deallocate(s as PoolIndex),
                    "第 {step} 步：归还池 {p} 的槽位 {s} 被拒绝"
                );
                allocated[p] &= !(1u64 << s);
                free_ops += 1;
                last_free = Option::Some((p, s));
            } else {
                skipped += 1;
            }
        } else {
            // 分配：沿池链轮转，跳过已满的池
            let mut done = false;
            for _ in 0..POOL_NUM {
                let p = cursor % POOL_NUM;
                cursor += 1;
                // SAFETY: owners[p] 指向独占分配的池
                let pool = unsafe { owners[p].as_mut() };
                if let Option::Some(slot) = pool.allocate() {
                    let slot = slot as usize;
                    assert!(
                        allocated[p] & (1u64 << slot) == 0,
                        "第 {step} 步：池 {p} 返回了账本中已占用的槽位 {slot}"
                    );
                    allocated[p] |= 1u64 << slot;
                    alloc_ops += 1;
                    last_alloc = Option::Some((p, slot));
                    done = true;
                    break;
                }
            }
            if !done {
                skipped += 1;
            }
        }

        // 每一步之后都校验整条链上所有池的内部数据
        check_chain_(&mut owners, &allocated, cap);
    }

    // 3) 收尾：轮流把所有池从半满分配至全满，验证空闲链表交出了每一个空闲槽位
    // 3) 收尾：轮流把所有池从当前状态分配至全满，验证空闲链表交出了每一个空闲槽位。
    // 随机阶段结束时各池的占用数已经随机化，所以这里先按账本算出应当还能放出多少。
    let expected_free: usize = allocated
        .iter()
        .map(|bits| cap - bits.count_ones() as usize)
        .sum();
    let mut filled = 0usize;
    loop {
        let mut progressed = false;
        for (p, owner) in owners.iter_mut().enumerate() {
            // SAFETY: owner 指向独占分配的池
            let pool = unsafe { owner.as_mut() };
            if let Option::Some(slot) = pool.allocate() {
                let slot = slot as usize;
                assert!(
                    allocated[p] & (1u64 << slot) == 0,
                    "放空阶段：池 {p} 返回了账本中已占用的槽位 {slot}"
                );
                allocated[p] |= 1u64 << slot;
                filled += 1;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
        check_chain_(&mut owners, &allocated, cap);
    }
    assert_eq!(
        filled, expected_free,
        "放空阶段放出的槽位数与账本记录的空闲槽位数不符"
    );
    for (p, bits) in allocated.iter().enumerate() {
        assert_eq!(bits.count_ones() as usize, cap, "池 {p} 应当已被放空");
    }

    // 4) 收尾之后所有池都满了，再也分配不出来
    for (p, owner) in owners.iter_mut().enumerate() {
        // SAFETY: owner 指向独占分配的池
        let pool = unsafe { owner.as_mut() };
        assert_eq!(pool.allocate(), Option::None, "池 {p} 已满却仍能分配");
        // 每个池都应当仍然认得住自己的域
        assert_eq!(
            pool.root().map(|r| r.as_ptr()),
            Option::Some(root.as_ptr()),
            "池 {p} 的域在随机操作后丢失"
        );
    }

    std::println!(
        "池链随机测试：{OPS} 步，分配 {alloc_ops} 次，归还 {free_ops} 次，跳过 {skipped} 次；\
         最后一步分配 {:?}，最后一步归还 {:?}",
        last_alloc,
        last_free,
    );

    // 这些池在这里故意不释放：测试进程随即退出，泄漏它们比手工重算布局再
    // 逐个 deallocate 更不容易出错；真正需要归还内存的场景由 Scope 负责。
    drop(owners);
}

/// 校验整条池链：链结构本身 + 每个池的空闲链表与其外部账本。
fn check_chain_(owners: &mut [NonNull<TestPool>], allocated: &[u64], cap: usize) {
    // 链结构：从第一个池出发，应当恰好访问到全部池各一次，且回指对称
    let mut visited = vec![false; owners.len()];
    let mut cursor = owners[0];
    let mut walked = 0usize;
    loop {
        let index = owners
            .iter()
            .position(|owner| owner.as_ptr() == cursor.as_ptr())
            .expect("池链行走到了不属于本链的池");
        assert!(!visited[index], "池链出现环：池 {index} 被访问了两次");
        visited[index] = true;
        walked += 1;

        // SAFETY: cursor 指向链上存活的池
        let pool = unsafe { cursor.as_ref() };
        match pool.next() {
            Option::Some(next) => {
                // SAFETY: next 指向链上存活的池
                let next_ref = unsafe { next.as_ref() };
                assert_eq!(
                    next_ref.prev().map(|p| p.as_ptr()),
                    Option::Some(cursor.as_ptr()),
                    "池 {index} 的后继没有正确回指"
                );
                cursor = next;
            }
            Option::None => break,
        }
    }
    assert_eq!(walked, owners.len(), "池链长度与预期不符");
    // 第一个池没有前驱
    // SAFETY: owners[0] 指向存活的池
    assert!(unsafe { owners[0].as_ref() }.prev().is_none(), "链头不应有前驱");

    let expected_root = unsafe { owners[0].as_ref() }.root();
    for (p, owner) in owners.iter_mut().enumerate() {
        // SAFETY: owner 指向独占分配的池
        let pool = unsafe { owner.as_mut() };
        assert_eq!(
            pool.root().map(|r| r.as_ptr()),
            expected_root.map(|r| r.as_ptr()),
            "池 {p} 的域绑定与其他池不一致"
        );
        check_pool_invariants_(pool, allocated[p], cap);
    }
}

/// 校验单个池：`used_count` / `free_count` 与账本一致，且从空闲链表表头出发能够恰好
/// 走遍账本中标为空闲的全部槽位，不丢、不重、不越界、不成环。
fn check_pool_invariants_(pool: &mut TestPool, allocated: u64, cap: usize) {
    let used = allocated.count_ones() as usize;
    assert_eq!(pool.used_count() as usize, used, "used_count 与账本不符");
    assert_eq!(pool.free_count() as usize, cap - used, "free_count 与账本不符");

    let free_mask = !allocated & (u64::MAX >> (64 - cap));
    if used == cap {
        assert_eq!(last_free_index_(pool), Option::None, "池已满却仍报告空闲");
        return;
    }

    // 顺着空闲链表走一遍，收集所有被串起来的槽位。
    // 链尾用 `capacity` 作标记，因此走到它即表示链表正常结束。
    let head = pool.latest_free();
    assert!(head < cap as PoolIndex, "空闲链表表头越界：{head}");
    let mut visited = 0u64;
    let mut cursor = head;
    let mut steps = 0usize;
    while cursor < cap as PoolIndex {
        let bit = 1u64 << cursor;
        assert_eq!(visited & bit, 0, "空闲链表出现环：槽位 {cursor} 被重复串入");
        visited |= bit;

        assert_eq!(
            pool.pool_order_of(cursor),
            cursor,
            "空闲槽位 {cursor} 记录的 pool_order 与池内下标不符"
        );

        cursor = pool.next_freed_of(cursor);
        steps += 1;
        assert!(steps <= cap, "空闲链表长度超过容量：{steps}");
    }
    assert_eq!(
        cursor as usize, cap,
        "空闲链表没有以容量值收尾，链尾标记异常"
    );

    assert_eq!(
        visited, free_mask,
        "空闲链表串起的槽位与账本记录的空闲槽位不一致"
    );
}

/// 池满时链表已经没有表头可言，这里返回 `None`；未满时返回当前表头。
fn last_free_index_(pool: &mut TestPool) -> Option<PoolIndex> {
    if pool.used_count() == pool.capacity() {
        Option::None
    } else {
        Option::Some(pool.latest_free())
    }
}


/// 反向验证不变式检查器本身是有效的：人为把空闲链表的表头改成一个越界下标之后，
/// `check_pool_invariants_` 必须报错，而不是默默放过。
/// - 手段：构造容量 16 的池并分配 8 个槽位；用测试专用钩子 `set_next_freed_of`
///   把表头的后继改写成 `容量 + 4`，再在 `catch_unwind` 里调用校验函数。
/// - 判断：校验函数必须 panic（即被 `catch_unwind` 捕获）。若它返回而不报错，说明
///   上面那条随机测试的判定标准是失效的，属于"测试没测到东西"的反向保护。
#[test]
fn weak_chunk_pool_invariant_checker_detects_corruption() {
    let mut owner = new_pool_();
    // SAFETY: owner 由 new_pool_ 独占分配，非空且在测试期间一直存活
    let pool = unsafe { owner.as_mut() };
    for _ in 0..(TEST_CAP / 2) {
        assert!(pool.allocate().is_some());
    }
    // 账本：低 8 位已占用，空闲槽位是 8..16
    let allocated = (1u64 << (TEST_CAP / 2)) - 1;
    check_pool_invariants_(pool, allocated, TEST_CAP as usize);

    let head = pool.latest_free();
    pool.set_next_freed_of(head, TEST_CAP + 4);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_pool_invariants_(pool, allocated, TEST_CAP as usize)
    }))
    .is_err();
    assert!(caught, "损坏的空闲链表没有被不变式检查器发现");
}
