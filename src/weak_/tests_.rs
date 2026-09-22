//! 内部单元测试。
//!
//! 这里的测试对象都是 crate 私有项（`WeakChunk` / `WeakChunkPool` 等），
//! 无法从 `tests/` 目录下的集成测试访问，因此集中放在本模块内。
//! `demo_.rs` 只作为使用示例，不承担测试职责。

use alloc::{alloc::Global, vec, vec::Vec};
use core::{
    mem::MaybeUninit,
    ptr::NonNull,
    sync::atomic::Ordering,
};

use crate::scope_inner_::{PoolIndex, ScopeInner};
use crate::weak_::{DataState, WeakChunk, WeakPool};

/// 测试统一使用"以 `ScopeInner<8>` 为根"的弱池；`Root` 参数在池内部是不透明的。
type WeakChunkPool<const CELL_SIZE: usize> = WeakPool<CELL_SIZE, ScopeInner<CELL_SIZE>>;

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

    // 64 位平台上的目标布局（Batch B 之后）：
    // 8(状态) + 8(prev_live) + 8(next_live) + 8(strong_chunk) + 8(record) + 8(meta) = 48
    #[cfg(target_pointer_width = "64")]
    assert_eq!(
        base_size, 48,
        "64 位平台上弱槽位应为 48 字节（原先每槽一份 24 字节的 DropVtable，现为一个记录指针 + 元数据）"
    );
}

/// 验证 `WeakPool` 的容量换算与其"池头按槽位取整"的布局一致。
/// - 手段：比较 `min_size_for_max_count` 与 `max_count_within_max_size` 的往返结果，
///   并把池头字节数取为"槽位大小的整数倍"；预算边界取"刚好放下 count 个槽位"与
///   "少一个字节"两种。
/// - 判断：容量到尺寸的换算必须恰好等于"取整后的池头 + count 个槽位"，少一个字节就要
///   少放一个槽位；零容量时也应占满取整后的池头。
#[test]
fn weak_chunk_pool_capacity_math() {
    use core::mem::size_of;

    type Pool = WeakChunkPool<8>;

    let slot = size_of::<WeakChunk<()>>();
    // 池头按槽位个数向上取整，槽位起点因此始终对齐
    assert_eq!(Pool::min_size_for_max_count(0), size_of::<Pool>().div_ceil(slot) * slot);

    for count in [1u16, 7, 1000, PoolIndex::MAX] {
        let need = Pool::min_size_for_max_count(count);
        let header = size_of::<Pool>().div_ceil(slot) * slot;
        assert_eq!(need, header + count as usize * slot);
        // 恰好足够的预算应换回同样的槽位数
        assert_eq!(Pool::max_count_within_max_size(need), count as usize);
        // 少一个字节就再也放不下这么多槽位
        assert_eq!(Pool::max_count_within_max_size(need - 1), count as usize - 1);
    }

    // 预算连池头都放不下时没有任何槽位可用
    let header = size_of::<Pool>().div_ceil(slot) * slot;
    assert_eq!(Pool::max_count_within_max_size(0), 0);
    assert_eq!(Pool::max_count_within_max_size(header), 0);
    assert_eq!(Pool::max_count_within_max_size(header - 1), 0);
    // 刚好放下一个槽位的预算
    assert_eq!(Pool::max_count_within_max_size(header + slot), 1);
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
    let slots = unsafe { pool.slots().as_mut() };
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
        let slots = unsafe { pool.slots().as_mut() };
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
        let slots = unsafe { pool.slots().as_mut() };
        let chunk = &slots[index as usize];
        chunk.incr_use_count();
        chunk.incr_use_count();
        assert_eq!(chunk.weak_count(), 2);
    }
    assert!(pool.deallocate(index));
    assert_eq!(pool.used_count(), 0);
    // SAFETY: 与上段相同，index 在范围内
    let slots = unsafe { pool.slots().as_mut() };
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
    let slots = unsafe { pool.slots().as_mut() };
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
    let slots = unsafe { pool.slots().as_mut() };
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

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 状态锁
// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----

/// 验证状态锁的尝试语义与守卫的自动释放。
/// - 手段：造一个槽位，先用带重试上限的 `try_lock` 取得守卫；在守卫存活期间再尝试一次
///   （另一个借用），随后析构守卫，再尝试第三次。
/// - 判断：第一次必须成功；守卫存活期间的第二次必须失败；守卫析构后的第三次必须成功，
///   且锁位确实被释放。这条测试同时说明守卫是"持有即独占、析构即归还"的。
#[test]
fn weak_chunk_state_lock_is_exclusive_and_released_by_guard() {
    let mut slot = MaybeUninit::<WeakChunk<()>>::uninit();
    // SAFETY: 先把 1 个槽位初始化干净，再取可变引用
    let chunk = unsafe {
        let ptr = slot.as_mut_ptr();
        WeakChunk::<()>::init_slots(core::slice::from_raw_parts_mut(ptr, 1), 1);
        &mut *ptr
    };

    {
        let guard = chunk.try_lock(64).expect("空闲槽位应当能立刻上锁");
        // 守卫存活期间，别的持有者拿不到锁
        assert!(chunk.try_lock(4).is_none(), "已上锁的槽位不应再次成功上锁");
        assert_eq!(guard.pool_order(), 0);
    }
    // 守卫析构后锁位应当被释放，且状态字没有残留垃圾位
    assert!(chunk.try_lock(64).is_some(), "守卫析构后应当能重新上锁");
    assert_eq!(chunk.data_state(), DataState::Reclaimed);
    assert_eq!(chunk.weak_count(), 0);
}

/// 验证忙等版加锁（`lock_busy`）在无竞争时同样可用。
/// - 手段：对空闲槽位调用 `lock_busy` 取得守卫，检查能通过 `Deref` 读到槽位字段。
/// - 判断：能拿到守卫并读到正确的 `pool_order`；守卫析构后 `try_lock` 仍能成功。
#[test]
fn weak_chunk_lock_busy_acquires_and_releases() {
    let mut slot = MaybeUninit::<WeakChunk<()>>::uninit();
    // SAFETY: 初始化 1 个槽位后取可变引用
    let chunk = unsafe {
        let ptr = slot.as_mut_ptr();
        WeakChunk::<()>::init_slots(core::slice::from_raw_parts_mut(ptr, 1), 1);
        &mut *ptr
    };

    {
        let guard = chunk.lock_busy();
        assert_eq!(guard.pool_order(), 0);
    }
    assert!(chunk.try_lock(64).is_some(), "忙等守卫析构后应当释放锁");
}

/// 验证锁在并发下确实互斥：多线程各自"取锁—进入临界区—放锁"，临界区不重叠。
/// - 手段：用一个槽位，8 个线程各重复 64 次 `lock_busy`；临界区内递增一个记录
///   "当前在临界区内的线程数"的原子计数（超过 1 就记一次溢出），再递减、放锁；
///   同时统计总进入次数。
/// - 判断：溢出次数必须为 0（互斥成立），总进入次数必须等于 8 × 64（没有线程饿死）。
///   若锁失效，两个线程会同时看到计数为 1，溢出计数大于 0。
#[test]
fn weak_chunk_state_lock_is_mutually_exclusive_across_threads() {
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize;
    use std::thread;

    const THREADS: usize = 8;
    const ROUNDS: usize = 64;

    /// 把裸指针包起来以便跨线程传递；真正的同步由槽位状态锁负责。
    struct SharedPtr(*mut WeakChunk<()>);
    // SAFETY: 各线程只在持有状态锁时解引用该指针
    unsafe impl Send for SharedPtr {}
    // SAFETY: 同上；锁保证同一时刻只有一个线程在临界区内
    unsafe impl Sync for SharedPtr {}

    let mut slot = MaybeUninit::<WeakChunk<()>>::uninit();
    // SAFETY: 先初始化这一个槽位，之后各线程只通过共享引用访问它
    let shared = unsafe {
        let ptr = slot.as_mut_ptr();
        WeakChunk::<()>::init_slots(core::slice::from_raw_parts_mut(ptr, 1), 1);
        Arc::new(SharedPtr(ptr))
    };

    let inside = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let overflowed = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let shared = Arc::clone(&shared);
        let inside = Arc::clone(&inside);
        let entered = Arc::clone(&entered);
        let overflowed = Arc::clone(&overflowed);
        handles.push(thread::spawn(move || {
            // SAFETY: 指针在整个测试期间有效；访问由状态锁串行化
            let chunk = unsafe { &*shared.0 };
            for _ in 0..ROUNDS {
                let guard = chunk.lock_busy();
                if inside.fetch_add(1, Ordering::AcqRel) + 1 > 1 {
                    overflowed.fetch_add(1, Ordering::AcqRel);
                }
                entered.fetch_add(1, Ordering::AcqRel);
                inside.fetch_sub(1, Ordering::AcqRel);
                drop(guard);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("线程不应 panic");
    }

    assert_eq!(
        overflowed.load(Ordering::Acquire),
        0,
        "临界区内同时出现了多个线程，互斥失败"
    );
    assert_eq!(
        entered.load(Ordering::Acquire),
        THREADS * ROUNDS,
        "有线程没能进入临界区（饥饿或丢锁）"
    );
}

/// 验证守卫的析构是唯一的解锁点，且临界区内 panic 也不会把锁漏掉。
/// - 手段：在 `catch_unwind` 里取得守卫并主动 panic，让栈回退触发守卫的 `Drop`；
///   随后在外部再尝试上锁。
/// - 判断：panic 被捕获后，槽位必须能重新上锁，且此前无人调用过任何"解锁方法"
///   （代码里根本不存在这样的入口）。若解锁只靠显式调用，这里会永久锁死。
#[test]
fn weak_chunk_lock_is_released_when_guard_drops_during_panic() {
    let mut slot = MaybeUninit::<WeakChunk<()>>::uninit();
    // SAFETY: 初始化 1 个槽位后取可变引用
    let chunk = unsafe {
        let ptr = slot.as_mut_ptr();
        WeakChunk::<()>::init_slots(core::slice::from_raw_parts_mut(ptr, 1), 1);
        &mut *ptr
    };

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = chunk.try_lock(64).expect("空闲槽位应当能上锁");
        panic!("在临界区内 panic，检查锁是否会被守卫带走");
    }));
    assert!(caught.is_err(), "panic 应当被 catch_unwind 捕获");

    // 守卫随栈回退析构，锁必须已经释放
    assert!(
        chunk.try_lock(64).is_some(),
        "临界区 panic 后锁必须已被守卫释放"
    );
}

/// 验证 `DataState` 的完整 7 态迁移路径与"认领唯一"。
/// - 手段：造一个 `Created` 槽位，依次走
///   `Created → Owning → Destroying → Destroyed → Finalized`，并在每一步检查状态与
///   重复调用的结果。
/// - 判断：`Owning` 之后再次升级必须失败；`Destroying` 之后再次 `try_claim_destroy`
///   必须失败（认领唯一，保证恰好析构一次）；`Destroyed` / `Finalized` 的状态值必须
///   与定义一致；整个过程中弱计数不受影响。
#[test]
fn weak_chunk_data_state_full_transition_path() {
    let mut slot = MaybeUninit::<WeakChunk<()>>::uninit();
    // SAFETY: 初始化 1 个槽位后取可变引用
    let chunk = unsafe {
        let ptr = slot.as_mut_ptr();
        WeakChunk::<()>::init_slots(core::slice::from_raw_parts_mut(ptr, 1), 1);
        &mut *ptr
    };
    let state = &chunk.chunk_state();

    assert_eq!(state.data_state(), DataState::Reclaimed);
    assert!(!state.data_state().is_data_alive());

    // Created → Owning
    state.init_created();
    assert_eq!(state.data_state(), DataState::Created);
    assert!(state.data_state().is_data_alive());
    assert!(state.try_set_state(DataState::Owning).is_some());
    assert_eq!(state.data_state(), DataState::Owning);

    // Owning 之后不能再被认领成 Sharing / Created
    assert!(state.try_set_state(DataState::Sharing).is_none());
    assert!(state.try_transition_state(DataState::Created, DataState::Sharing).is_none());

    // Owning → Destroying → Destroyed → Finalized
    assert_eq!(state.try_claim_destroy(), Some(DataState::Owning));
    assert_eq!(state.data_state(), DataState::Destroying);
    assert!(!state.data_state().is_data_alive(), "Destroying 不算存活");
    assert_eq!(state.try_claim_destroy(), None, "销毁认领必须唯一");

    state.mark_destroyed();
    assert_eq!(state.data_state(), DataState::Destroyed);
    state.mark_finalized();
    assert_eq!(state.data_state(), DataState::Finalized);

    // 全程不影响弱计数
    assert_eq!(state.weak_count(), 0);
    assert_eq!(state.pool_order(), 0);
}

/// 测试"由槽位地址 O(1) 反推所属弱池"。
/// - 手段：构造两个容量 16 的池，各自把全部槽位分配掉，再对每个槽位调用
///   `WeakPool::of_slot_`；每个池的槽位都应反推回它自己的首址。
/// - 判断：任一槽位反推出的池地址与该槽位实际所属的池不同即断言失败；两个池的槽位互不
///   串味（池 A 的槽位不会反推到池 B），说明 `pool_order_` 与池头偏移的换算正确。
#[test]
fn weak_pool_of_slot_reverses_to_owning_pool() {
    let pools = [new_pool_(), new_pool_()];
    for mut owner in pools {
        // SAFETY: owner 指向独占分配的池；测试结束前都存活
        let pool = unsafe { owner.as_mut() };
        let owner_ptr = owner.as_ptr();
        for _ in 0..TEST_CAP {
            pool.allocate().expect("按容量逐个分配应当成功");
        }
        // SAFETY: 池未释放，槽位数组有效
        let slots = unsafe { pool.slots().as_mut() };
        for index in 0..TEST_CAP as usize {
            let slot = NonNull::from(&mut slots[index]);
            let derived = TestPool::of_slot_(slot).expect("槽位必须能反推出所属池");
            assert_eq!(
                derived.as_ptr(),
                owner_ptr,
                "槽位 {index} 反推出的池不是它实际所属的池",
            );
        }
    }
}
