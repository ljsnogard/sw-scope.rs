//! 内部单元测试。
//!
//! 这里的测试对象都是 crate 私有项（`WeakChunk` / `WeakChunkPool` 等），
//! 无法从 `tests/` 目录下的集成测试访问，因此集中放在本模块内。
//! `demo_.rs` 只作为使用示例，不承担测试职责。

use alloc::alloc::Global;

use crate::scope_inner_::{PoolIndex, WeakChunkPool};
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
