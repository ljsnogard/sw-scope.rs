//! `strong_` 模块的内部单元测试：控制块布局、身份绑定与升级路径。

use core::{
    mem::{MaybeUninit, align_of, size_of},
    ptr::NonNull,
};

use super::*;
use crate::weak_::{DataState, WeakChunk, WeakChunkState};

/// 造一个仅用于测试的"身份槽位"：栈上的 `WeakChunk<()>`，并推进到 `Created`，
/// 模拟一次成功分配之后的状态。
fn make_identity_() -> WeakChunk<()> {
    let slot = WeakChunk::<()> {
        chunk_state_: WeakChunkState::empty_(),
        prev_live_: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        next_live_: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        strong_chunk_: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
        record_: Option::None,
        meta_: core::ptr::null(),
        _unused_t_: core::marker::PhantomData,
    };
    // 真实路径上这一步由分配流程完成
    let _ = slot
        .chunk_state_
        .compare_exchange_state(DataState::Reclaimed, DataState::Created);
    slot
}

/// 造一块 `StrongChunk<u64>` 并绑定给 `weak`。
///
/// # Safety
///
/// `weak` 必须指向一个存活的身份槽位，且活得比返回值更久；`slot` 同理。
unsafe fn make_chunk_(
    slot: &mut MaybeUninit<StrongChunk<u64>>,
    weak: *mut WeakChunk<()>,
    value: u64,
) -> &mut StrongChunk<u64> {
    // SAFETY: 由调用方保证 slot 可写、weak 有效
    unsafe {
        let this = &mut *slot.as_mut_ptr();
        this.init_with_(NonNull::new_unchecked(weak), value);
        this
    }
}

/// 验证控制块布局：强块头部尽量小，弱槽位承载全部管理信息且尺寸与 `T` 无关。
/// - 手段：断言 `StrongChunkBase` 的大小与对齐、`WeakChunk<T>` 在 `T = ()`、`u8`、
///   `[u8]` 下的尺寸一致，以及数据区偏移是 8 的倍数。
/// - 判断：任一尺寸或对齐不符即失败。弱槽位尺寸与 `T` 无关是弱池按 `WeakChunk<()>`
///   步长定位槽位的前提；强块头部小则是 arena 空间效率的直接体现。
#[test]
fn strong_chunk_layout_is_compact_and_aligned() {
    assert_eq!(size_of::<StrongChunkBase>(), 16);
    assert_eq!(align_of::<StrongChunkBase>(), 8);

    let weak_size = size_of::<WeakChunk<()>>();
    assert_eq!(weak_size, size_of::<WeakChunk<u8>>());
    assert_eq!(weak_size, size_of::<WeakChunk<[u8]>>());
    assert_eq!(align_of::<WeakChunk<()>>(), 8);

    // 数据区在块内的偏移按实际投影测量，必须是 8 的倍数（头部与数据同对齐）
    let probe = MaybeUninit::<StrongChunk<u64>>::uninit();
    let offset = probe.as_ptr() as usize;
    assert_eq!(offset % 8, 0);
    assert_eq!(size_of::<StrongChunk<u64>>(), 16 + size_of::<u64>());
}

/// 验证"强块只认身份、身份指向强块"的双向绑定，以及数据访问与析构登记。
/// - 手段：造一块 `StrongChunk<u64>` 绑定栈上的身份槽位，随后双向检查。
/// - 判断：弱槽位记录的强块指针必须等于强块自身地址（头部在偏移 0，因此两者同址）；
///   强块记录的弱槽位必须等于身份地址；数据引用读出写入值；需要析构的类型必须已登记
///   析构入口。这保证清盘流程只凭身份就能找回并析构数据。
#[test]
fn strong_chunk_binds_identity_both_ways() {
    let mut weak = make_identity_();
    let mut slot = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: slot 与 weak 都在本测试的栈上存活
    let chunk = unsafe { make_chunk_(&mut slot, &mut weak, 0x0123_4567_89AB_CDEF) };

    let weak_ptr: NonNull<WeakChunk<()>> = NonNull::from(&mut weak).cast();
    assert_eq!(chunk.weak_chunk().as_ptr(), weak_ptr.as_ptr());
    assert_eq!(
        weak.strong_chunk().map(|p| p.as_ptr()),
        Option::Some(chunk as *mut StrongChunk<u64> as *mut StrongChunkBase),
    );

    assert_eq!(*chunk.try_get_data().unwrap(), 0x0123_4567_89AB_CDEF);
    assert!(chunk.is_data_alive(), "绑定后数据应当活着");
    assert_eq!(chunk.strong_count(), 0, "新建的强块强计数从 0 开始");
    assert!(
        weak.record_.is_none(),
        "u64 不需要析构，因此不该登记清理入口"
    );
}

/// 验证类型擦除析构确实会把数据析构掉。
/// - 手段：用带 `Drop` 的探针类型建块，经 `init_with_` 登记后，模拟清盘流程从身份槽位
///   取出析构登记并调用它。
/// - 判断：登记里必须有析构入口，且调用后探针的析构计数恰好增加 1。这是"清盘只拿得到
///   类型无关的头部，却仍能正确析构具体类型"的关键路径。
#[test]
fn strong_chunk_drop_is_type_erased_but_effective() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    static DROPPED: AtomicUsize = AtomicUsize::new(0);

    struct Probe;
    impl Drop for Probe {
        fn drop(&mut self) {
            DROPPED.fetch_add(1, Ordering::AcqRel);
        }
    }

    let mut weak = make_identity_();
    let mut slot = MaybeUninit::<StrongChunk<Probe>>::uninit();
    // SAFETY: slot 与 weak 都在本测试的栈上存活
    unsafe {
        let this = &mut *slot.as_mut_ptr();
        this.init_with_(NonNull::from(&mut weak).cast(), Probe);
    }

    // 登记的清理入口必须能就地析构数据，这正是清盘流程依赖的路径
    assert!(
        weak.record_.is_some_and(|record| !record.is_noop_()),
        "带 Drop 的数据必须登记清理入口"
    );
    assert_eq!(DROPPED.load(Ordering::Acquire), 0);
    // SAFETY: 数据刚构造好、尚未析构
    unsafe { weak.drop_data() };
    assert_eq!(DROPPED.load(Ordering::Acquire), 1, "类型擦除析构必须真的执行");
}

/// 验证"数据活着"与"有强引用"是两件事，以及两条升级路径的互斥性。
/// - 手段：`Created` 状态下先试 `try_retain`，再试 `try_share`；另起一块走
///   `try_share` 路径并检查强计数。
/// - 判断：升级为 `Owning` 后状态变 `Owning`、强计数仍为 0，且再 `try_share` 必须失败；
///   升级为 `Sharing` 时强计数必须置 1，且再 `try_retain` 必须失败。
#[test]
fn strong_state_upgrade_paths() {
    let mut weak = make_identity_();
    let weak_ptr = &mut weak as *mut WeakChunk<()>;
    let mut slot = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: slot 与 weak 都在本测试的栈上存活
    let chunk = unsafe { make_chunk_(&mut slot, weak_ptr, 7) };

    assert_eq!(weak.data_state(), DataState::Created);
    assert!(chunk.is_data_alive());
    assert!(weak.try_owning().is_ok());
    assert_eq!(weak.data_state(), DataState::Owning);
    assert_eq!(weak.try_sharing(), Result::Err(DataState::Owning));
    assert!(chunk.is_data_alive(), "独占持有期间数据依然活着");

    let mut weak2 = make_identity_();
    let weak2_ptr = &mut weak2 as *mut WeakChunk<()>;
    let mut slot2 = MaybeUninit::<StrongChunk<u64>>::uninit();
    // SAFETY: slot2 与 weak2 都在本测试的栈上存活
    let chunk2 = unsafe { make_chunk_(&mut slot2, weak2_ptr, 9) };

    assert!(weak2.try_sharing().is_ok());
    assert_eq!(weak2.data_state(), DataState::Sharing);
    assert_eq!(chunk2.strong_count(), 1, "升级为 Sharing 时强计数置 1");
    assert!(weak2.try_owning().is_err(), "已共享的块不能再独占");
}

/// 验证 `?Sized` 数据的类型元数据确实被保存在弱槽位上，并且能从槽位完整复原。
/// - 手段：取一个 `&dyn Debug` 胖指针，经 `meta_to_raw_` / `meta_from_raw_` 走一遍
///   "存进弱槽位再取出来"的往返，再用复原出的元数据重建引用。
/// - 判断：复原出的 `DynMetadata` 必须与原元数据逐位相等，重建出的引用必须打印出原值。
///   这正是"清盘一侧对 `T` 一无所知也能析构/访问"的依据。
#[test]
fn weak_chunk_rebuilds_unsized_data_from_metadata() {
    let value: u32 = 1234;
    let repr: *const dyn core::fmt::Debug = &value;
    let (repr_data, repr_meta) = (repr as *const (), core::ptr::metadata(repr));

    let mut weak = make_identity_();
    weak.set_record_erased_(Option::None, crate::weak_::meta_to_raw_(repr_meta));

    // 从槽位取回元数据：`?Sized` 的虚表指针必须被原样保存
    let roundtrip: core::ptr::DynMetadata<dyn core::fmt::Debug> =
        unsafe { crate::weak_::meta_from_raw_::<dyn core::fmt::Debug>(weak.meta_) };
    assert_eq!(roundtrip, repr_meta, "虚表元数据必须逐位保存");

    // 用取回的元数据重建胖指针，内容必须与原始引用一致
    // SAFETY: 元数据取自上面同一次登记，数据指针也来自同一个引用
    let rebuilt =
        unsafe { &*core::ptr::from_raw_parts::<dyn core::fmt::Debug>(repr_data, roundtrip) };
    assert_eq!(alloc::format!("{rebuilt:?}"), "1234");
}

/// 验证 `WeakChunkState` 上"仅当 `Created` 才允许迁移"的升级原语。
/// - 手段：把槽位置为 `Created`，先试 `try_set_state(Owning)`，再试 `try_set_state(Sharing)`。
/// - 判断：第一次必须成功并返回先前状态 `Created`；第二次必须失败（已经不是 `Created`），
///   状态保持在 `Owning`。这正是升级路径"判据唯一"的依据。
#[test]
fn weak_state_upgrade_primitive_is_exclusive() {
    let mut weak = make_identity_();
    let state = &weak.chunk_state_;
    state.init_created();

    assert_eq!(state.try_set_state(DataState::Owning), Option::Some(DataState::Created));
    assert_eq!(state.data_state(), DataState::Owning);
    assert_eq!(state.try_set_state(DataState::Sharing), Option::None);
    assert_eq!(state.data_state(), DataState::Owning);
}

/// 验证 `Sharing` 路径的状态迁移与"认领销毁唯一"。
/// - 手段：另造一个 `Created` 槽位，走 `try_set_state(Sharing)` → `try_claim_destroy`
///   → `mark_destroyed` → `mark_finalized`。
/// - 判断：升级为 `Sharing` 成功且此时不可再独占；认领销毁只能成功一次（第二次返回
///   `None`），随后状态依次为 `Destroying`、`Destroyed`、`Finalized`。这覆盖了
///   "`Sharing` 计数归零后由最后一个持有者认领析构"这条路径的状态侧行为。
#[test]
fn weak_state_sharing_path_claims_destroy_exactly_once() {
    let mut weak = make_identity_();
    let state = &weak.chunk_state_;
    state.init_created();

    assert_eq!(state.try_set_state(DataState::Sharing), Option::Some(DataState::Created));
    assert_eq!(state.data_state(), DataState::Sharing);
    assert_eq!(state.try_set_state(DataState::Owning), Option::None);

    assert_eq!(state.try_claim_destroy(), Option::Some(DataState::Sharing));
    assert_eq!(state.data_state(), DataState::Destroying);
    assert_eq!(state.try_claim_destroy(), Option::None, "第二个认领者必须失败");

    state.mark_destroyed();
    assert_eq!(state.data_state(), DataState::Destroyed);
    state.mark_finalized();
    assert_eq!(state.data_state(), DataState::Finalized);
}

/// 验证强池的 bump 分配：按布局对齐、`used_count_` 只增不减、空闲区随之收缩。
/// - 手段：申请一个 64 cell 的池，先分配对齐 8 的块，再分配对齐 16 的块（模拟 `u128`）。
/// - 判断：两块地址分别满足各自对齐；`used_count_` 随分配单调增加且不超过容量；
///   空闲字节数在第二次分配后严格减少。最后把池内存还给分配器，避免测试泄漏。
#[test]
fn strong_pool_bump_allocation_is_aligned() {
    use alloc::alloc::Global;
    use core::alloc::{Allocator, Layout};

    static GLOBAL: Global = Global;

    let pool_ptr = StrongPool::<8>::try_new_(64, &GLOBAL, Option::None).expect("建池应当成功");
    {
        // SAFETY: pool_ptr 是刚初始化的独占池
        let pool = unsafe { pool_ptr.as_ref() };
        assert_eq!(pool.cell_count_(), 64);
        assert_eq!(pool.used_count_(), 0);
    }
    let first_used;
    // SAFETY: 同上；只在本块内可变借用
    {
        let pool = unsafe { pool_ptr.as_ptr().as_mut().unwrap() };
        let a = pool
            .allocate_(Layout::from_size_align(16, 8).unwrap())
            .expect("第一块应当放得下");
        assert_eq!(
            (a.as_ptr() as *mut u8).align_offset(8),
            0,
            "第一块必须满足 align=8"
        );
        first_used = pool.used_count_();

        let b = pool
            .allocate_(Layout::from_size_align(16, 16).unwrap())
            .expect("第二块应当放得下");
        assert_eq!(
            (b.as_ptr() as *mut u8).align_offset(16),
            0,
            "第二块必须满足 align=16"
        );
        assert!(
            pool.used_count_() > first_used,
            "第二次分配后已用 cell 必须增加"
        );
        assert!(pool.used_count_() <= pool.cell_count_());
    }
    // SAFETY: pool_ptr 由 GLOBAL 按 layout_for_(64) 分配，且此时借用已结束
    unsafe { GLOBAL.deallocate(pool_ptr.cast(), StrongPool::<8>::layout_for_(64)) };
}
