//! `strong_` 模块的内部单元测试：头部布局、工厂决定的形状、可选槽与升级路径。

use core::{
    alloc::Layout,
    mem::{align_of, size_of},
    ptr::{self, NonNull},
};

use super::*;
use crate::{
    abs_::TrPredrop,
    weak_::{DataState, WeakSlot},
};

/// 造一个身份槽位并推进到 `Created`，模拟一次成功分配。
fn make_identity_() -> WeakSlot<()> {
    let slot = WeakSlot::<()>::stack_empty_();
    let _ = slot
        .chunk_state()
        .try_transition_state(DataState::Reclaimed, DataState::Created);
    slot
}

/// 测试用的对齐缓冲区：头部 + meta 指针 + predrop + 数据都能放下。
#[repr(C, align(16))]
struct ChunkBuf<const N: usize>([usize; N]);

impl<const N: usize> ChunkBuf<N> {
    const fn new() -> Self {
        ChunkBuf([0usize; N])
    }

    fn base_(&mut self) -> *mut StrongChunkBase {
        self.0.as_mut_ptr().cast::<StrongChunkBase>()
    }
}

/// 非零尺寸的 Predrop 对象；直接存储，便于检查内存位置。
#[derive(Debug)]
struct Hook {
    value: u64,
}

impl<T: ?Sized> TrPredrop<T> for Hook {
    fn pre_drop(self, _data: &mut T) {
        let _ = self.value;
    }
}

/// 析构测试用探针。
struct Probe;
impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    }
}

struct ProbeHook;
impl TrPredrop<Probe> for ProbeHook {
    fn pre_drop(self, _data: &mut Probe) {
        PRE_DROPPED.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    }
}

static DROPPED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static PRE_DROPPED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 验证头部与默认视图的尺寸；真实数据位置由工厂给出的偏移决定。
#[cfg(target_pointer_width = "64")]
#[test]
fn strong_chunk_layout_is_compact_and_aligned() {
    assert_eq!(size_of::<StrongChunkBase>(), 16);
    assert_eq!(align_of::<StrongChunkBase>(), 8);
    // 视图本身只有头部 + 零尺寸类型标记，不包含数据。
    assert_eq!(size_of::<StrongChunk<u64>>(), size_of::<StrongChunkBase>());

    let weak_size = size_of::<WeakSlot<()>>();
    assert_eq!(weak_size, size_of::<WeakSlot<u8>>());
    assert_eq!(weak_size, size_of::<WeakSlot<[u8]>>());
    assert_eq!(align_of::<WeakSlot<()>>(), 8);

    let (_, dat_offset) =
        StrongChunk::<u64>::layout_sized(Layout::new::<u64>()).expect("布局不应溢出");
    assert_eq!(dat_offset, 2, "Sized、无 Predrop 的数据偏移应为两个指针");
}

/// 验证身份双向绑定与默认数据访问。
#[test]
fn strong_chunk_binds_identity_both_ways() {
    let mut weak = make_identity_();
    let mut buf = ChunkBuf::<8>::new();
    // SAFETY: buf 足够大；weak 在测试栈上存活
    let chunk = unsafe { &mut *(buf.base_() as *mut StrongChunk<u64>) };
    // SAFETY: 默认 Sized 布局
    unsafe {
        StrongChunk::<u64>::init_with_value(
            buf.base_() as *mut StrongChunk<u64>,
            NonNull::from(&mut weak).cast(),
            0x0123_4567_89AB_CDEF,
        )
    };

    let weak_ptr: NonNull<WeakSlot<()>> = NonNull::from(&mut weak).cast();
    assert_eq!(chunk.weak_chunk().as_ptr(), weak_ptr.as_ptr());
    assert_eq!(
        weak.strong_chunk().map(|p| p.as_ptr()),
        Option::Some(buf.base_()),
    );

    assert_eq!(*chunk.try_get_data().unwrap(), 0x0123_4567_89AB_CDEF);
    assert!(chunk.is_data_alive());
    assert_eq!(chunk.strong_count(), 0);
    assert!(chunk.predrop_().is_none());
    assert!(!chunk.has_meta_());
}

/// 验证 Sized 数据的两种 Predrop 形状：无 Predrop 与直接存储 Predrop 对象。
#[cfg(target_pointer_width = "64")]
#[test]
fn strong_chunk_sized_layout_with_optional_predrop() {
    let (_, off) = StrongChunk::<u64, ()>::layout_sized(Layout::new::<u64>()).unwrap();
    assert_eq!(off, 2);

    let (_, off) = StrongChunk::<u64, Hook>::layout_sized(Layout::new::<u64>()).unwrap();
    assert_eq!(off, 3);

    let mut weak = make_identity_();
    let mut buf = ChunkBuf::<8>::new();
    // SAFETY: buf 足够大；weak 在测试栈上存活
    let chunk = unsafe { &mut *(buf.base_() as *mut StrongChunk<u64, Hook>) };
    // SAFETY: 布局已计算
    unsafe {
        StrongChunk::<u64, Hook>::init_sized(
            buf.base_() as *mut StrongChunk<u64, Hook>,
            NonNull::from(&mut weak).cast(),
            9,
            Hook { value: 0xCAFE },
            off,
        )
    };
    // 数据写在 dat_offset 处。
    let data = chunk.data_ptr();
    // SAFETY: 数据区已对齐、尚未初始化
    unsafe { data.write(0xDEAD_BEEF) };

    assert!(!chunk.base_().has_meta_());
    assert!(chunk.base_().has_predrop_());
    assert_eq!(chunk.base_().pool_order_(), 9);
    assert_eq!(chunk.predrop_().expect("Predrop 槽").value, 0xCAFE);
    assert_eq!(
        &buf.0[2] as *const usize,
        &chunk.predrop_().unwrap().value as *const u64 as *const usize
    );
    assert_eq!(*chunk.try_get_data().unwrap(), 0xDEAD_BEEF);
}

/// 验证 `?Sized` 数据必须带 meta 指针，并且 Predrop 仍可独立选择。
#[cfg(target_pointer_width = "64")]
#[test]
fn strong_chunk_unsized_layout_has_meta_pointer() {
    let values = [7u64, 8, 9];
    let meta_word = crate::weak_::meta_to_raw_(ptr::metadata(&values[..]));
    let meta_addr = &meta_word as *const *const () as MetaAddr;

    let (_, off) =
        StrongChunk::<[u64], ()>::layout_unsized(Layout::for_value(&values[..])).unwrap();
    assert_eq!(off, 3, "base + meta 指针 = 3 个指针");

    let mut weak = make_identity_();
    let mut buf = ChunkBuf::<8>::new();
    // SAFETY: buf 足够大；weak 与 meta_word 在测试栈上存活
    let chunk = unsafe { &mut *(buf.base_() as *mut StrongChunk<[u64], ()>) };
    // SAFETY: 布局已计算
    unsafe {
        StrongChunk::<[u64], ()>::init_unsized(
            buf.base_() as *mut StrongChunk<[u64], ()>,
            NonNull::from(&mut weak).cast(),
            0,
            meta_addr,
            (),
            off,
        )
    };
    // SAFETY: 数据区已对齐、尚未初始化
    unsafe {
        ptr::copy_nonoverlapping(values.as_ptr(), chunk.data_ptr() as *mut u64, values.len())
    };

    assert!(chunk.has_meta_());
    assert!(!chunk.has_predrop_());
    assert_eq!(chunk.base_().meta_addr_(), Option::Some(meta_addr));
    assert_eq!(chunk.try_get_data().unwrap(), &values[..]);
}

/// 验证 base -> meta -> predrop -> data 的实际顺序，以及 Predrop 直接存储。
#[cfg(target_pointer_width = "64")]
#[test]
fn strong_chunk_meta_and_predrop_are_laid_out_before_data() {
    let value = 0x0123_4567_89AB_CDEF_0123_4567_89AB_CDEFu128;
    let meta_word = crate::weak_::meta_to_raw_(1usize);
    let meta_addr = &meta_word as *const *const () as MetaAddr;
    let (_, off) = StrongChunk::<[u128], Hook>::layout_unsized(Layout::for_value(&[value][..]))
        .expect("布局不应溢出");
    assert_eq!(off, 4, "base + meta + predrop = 4 个指针");

    let mut weak = make_identity_();
    let mut buf = ChunkBuf::<8>::new();
    // SAFETY: buf 足够大；weak/meta_word 在测试栈上存活
    let chunk = unsafe { &mut *(buf.base_() as *mut StrongChunk<[u128], Hook>) };
    // SAFETY: 布局已计算
    unsafe {
        StrongChunk::<[u128], Hook>::init_unsized(
            buf.base_() as *mut StrongChunk<[u128], Hook>,
            NonNull::from(&mut weak).cast(),
            3,
            meta_addr,
            Hook { value: 0xBEEF },
            off,
        )
    };
    // SAFETY: 数据区已对齐、尚未初始化
    unsafe { ptr::write(chunk.data_ptr() as *mut u128, value) };

    assert_eq!(buf.0[2], meta_addr as usize);
    assert_eq!(
        &buf.0[3] as *const usize,
        &chunk.predrop_().unwrap().value as *const u64 as *const usize
    );
    assert_eq!(chunk.predrop_().unwrap().value, 0xBEEF);
    assert_eq!(chunk.try_get_data().unwrap(), &[value][..]);
}

/// 验证 typed 析构：Predrop 对象先执行，再析构数据。
#[test]
fn strong_chunk_drop_runs_predrop_before_data_drop() {
    use core::sync::atomic::Ordering;

    DROPPED.store(0, Ordering::Release);
    PRE_DROPPED.store(0, Ordering::Release);

    let (_, off) = StrongChunk::<Probe, ProbeHook>::layout_sized(Layout::new::<Probe>())
        .expect("布局不应溢出");
    let mut weak = make_identity_();
    let mut buf = ChunkBuf::<8>::new();
    // SAFETY: buf 足够大；weak 在测试栈上存活
    let chunk = unsafe { &mut *(buf.base_() as *mut StrongChunk<Probe, ProbeHook>) };
    // SAFETY: 布局已计算
    unsafe {
        StrongChunk::<Probe, ProbeHook>::init_sized(
            buf.base_() as *mut StrongChunk<Probe, ProbeHook>,
            NonNull::from(&mut weak).cast(),
            0,
            ProbeHook,
            off,
        )
    };
    // SAFETY: 数据刚构造好、尚未析构
    unsafe { chunk.drop_data_() };
    assert_eq!(
        PRE_DROPPED.load(Ordering::Acquire),
        1,
        "Predrop 必须先执行一次"
    );
    assert_eq!(DROPPED.load(Ordering::Acquire), 1, "数据必须恰好析构一次");
}

/// 验证 `pool_order_` 能在归还路径上 O(1) 反推所属强池。
#[test]
fn strong_chunk_pool_order_reverses_to_pool() {
    use core::alloc::Allocator;
    use std::alloc::Global;

    static GLOBAL: Global = Global;

    let mut pool_ptr = StrongPool::try_new_(64, &GLOBAL, Option::None).expect("建池应当成功");
    // SAFETY: pool_ptr 由本测试独占
    let pool = unsafe { pool_ptr.as_mut() };
    let (mem, order) = pool
        .allocate_with_order_(Layout::from_size_align(32, 8).expect("布局合法"))
        .expect("池内应当放得下");
    let mut weak = make_identity_();
    let (_, dat_offset) = StrongChunk::<u64>::layout_sized(Layout::new::<u64>()).unwrap();
    // SAFETY: 分配块至少 32 字节；weak 在测试栈上存活
    let base = mem.as_ptr() as *mut StrongChunk<u64>;
    unsafe {
        StrongChunk::<u64>::init_sized(base, NonNull::from(&mut weak).cast(), order, (), dat_offset)
    };
    let chunk_base = NonNull::new(mem.as_ptr() as *mut StrongChunkBase).expect("块地址非空");
    let derived = StrongPool::of_chunk_(chunk_base).expect("必须能由 pool_order_ 反推池");
    assert_eq!(derived.as_ptr(), pool_ptr.as_ptr());

    // SAFETY: 池由 GLOBAL 按 layout_for_(64) 分配，且测试独占
    unsafe { GLOBAL.deallocate(pool_ptr.cast(), StrongPool::layout_for_(64)) };
}

/// 验证 `WeakChunkState` 上"仅当 `Created` 才允许迁移"的升级原语。
#[test]
fn weak_state_upgrade_primitive_is_exclusive() {
    let weak = make_identity_();
    let state = &weak.chunk_state();
    state.init_created();
    assert_eq!(
        state.try_set_state(DataState::Owning),
        Option::Some(DataState::Created)
    );
    assert_eq!(state.data_state(), DataState::Owning);
    assert_eq!(state.try_set_state(DataState::Sharing), Option::None);
}

/// 验证 `Sharing` 路径的状态迁移与"认领销毁唯一"。
#[test]
fn weak_state_sharing_path_claims_destroy_exactly_once() {
    let weak = make_identity_();
    let state = &weak.chunk_state();
    state.init_created();
    assert_eq!(
        state.try_set_state(DataState::Sharing),
        Option::Some(DataState::Created)
    );
    assert_eq!(state.try_claim_destroy(), Option::Some(DataState::Sharing));
    assert_eq!(state.try_claim_destroy(), Option::None);
    state.mark_destroyed();
    state.mark_finalized();
    assert_eq!(state.data_state(), DataState::Finalized);
}

/// 验证"数据活着"与"有强引用"是两件事，以及两条升级路径的互斥性。
#[test]
fn strong_state_upgrade_paths() {
    let mut weak = make_identity_();
    let mut buf = ChunkBuf::<8>::new();
    // SAFETY: buf 足够大；weak 在测试栈上存活
    let chunk = unsafe { &mut *(buf.base_() as *mut StrongChunk<u64>) };
    // SAFETY: 默认 Sized 布局
    unsafe {
        StrongChunk::<u64>::init_with_value(
            buf.base_() as *mut StrongChunk<u64>,
            NonNull::from(&mut weak).cast(),
            7,
        )
    };

    assert_eq!(weak.data_state(), DataState::Created);
    assert!(chunk.is_data_alive());
    assert!(weak.try_owning().is_ok());
    assert_eq!(weak.data_state(), DataState::Owning);
    assert_eq!(weak.try_sharing(), Result::Err(DataState::Owning));

    let mut weak2 = make_identity_();
    let mut buf2 = ChunkBuf::<8>::new();
    // SAFETY: buf2 足够大；weak2 在测试栈上存活
    let chunk2 = unsafe { &mut *(buf2.base_() as *mut StrongChunk<u64>) };
    // SAFETY: 默认 Sized 布局
    unsafe {
        StrongChunk::<u64>::init_with_value(
            buf2.base_() as *mut StrongChunk<u64>,
            NonNull::from(&mut weak2).cast(),
            9,
        )
    };
    assert!(weak2.try_sharing().is_ok());
    assert_eq!(chunk2.strong_count(), 1);
    assert!(weak2.try_owning().is_err());
}

/// 验证 `?Sized` 元数据的裸指针往返。
#[test]
fn weak_chunk_rebuilds_unsized_data_from_metadata() {
    let value: u32 = 1234;
    let repr: *const dyn core::fmt::Debug = &value;
    let (repr_data, repr_meta) = (repr as *const (), core::ptr::metadata(repr));
    let raw = crate::weak_::meta_to_raw_(repr_meta);
    let roundtrip: core::ptr::DynMetadata<dyn core::fmt::Debug> =
        unsafe { crate::weak_::meta_from_raw_::<dyn core::fmt::Debug>(raw) };
    assert_eq!(roundtrip, repr_meta);
    let rebuilt =
        unsafe { &*core::ptr::from_raw_parts::<dyn core::fmt::Debug>(repr_data, roundtrip) };
    assert_eq!(std::format!("{rebuilt:?}"), "1234");
}
