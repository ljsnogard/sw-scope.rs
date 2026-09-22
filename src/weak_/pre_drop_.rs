//! `PreDrop` 的类型级登记（[`PreDropRecord`]）与类型级注册表（[`PreDropRegistry`]）。
//!
//! 设计背景、决策日志与实验目标见 `dev-notes/weak-20260922-1135.md`。要点：
//!
//! - 每个具体 `T`（以及带对象级钩子的每个 `(T, Fin)`）只生成**一份**登记，通过"关联常量 +
//!   取引用"提升进静态内存；弱槽位只保存一个 `&'static PreDropRecord`；
//! - 记录自身只含一个类型擦除入口，**不含**数据地址与 `?Sized` 元数据：前者由入口按 `T`
//!   单态化重建，后者仍逐对象保存在弱槽位里；
//! - 类型级钩子由 `RootScope` 上的 [`PreDropRegistry`] 按类型名查找，对象级钩子直接在放入点
//!   生成记录并覆盖类型级；
//! - 钩子必须是**零尺寸类型**（函数项或非捕获闭包），捕获式闭包会在编译期被 `const` 断言拒绝。

use core::{mem, ptr};

use super::chunk_::data_ptr_of_;

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 记录

/// 一份"该类型的对象进入清理列表时怎么处理"的类型级登记：先跑可选 `PreDrop` 钩子，
/// 再析构数据。
///
/// 它只保存一个类型擦除入口，因此记录本身**每类型一份**即可；同一个 `T` 的所有对象共用它，
/// 不存在逐对象副本。`!needs_drop::<T>()` 且没注册钩子时入口为 [`None`]，清理时什么都不做。
#[repr(transparent)]
pub(crate) struct PreDropRecord {
    /// 类型擦除的清理入口：接收"强块首址 + `?Sized` 元数据"。
    ///
    /// `None` 表示该类型既不需要 `Drop`、也没有 `PreDrop` 钩子。
    entry_: Option<unsafe fn(*mut u8, *const ())>,
}

impl PreDropRecord {
    /// 无钩子的类型：只有 `needs_drop` 时才有入口。
    ///
    /// 同一个 `T` 在任何调用点都会取到同一个 `&'static PreDropRecord`。
    pub(crate) fn of<T: ?Sized>() -> &'static Self {
        trait HasRecord {
            const V: PreDropRecord;
        }
        impl<T: ?Sized> HasRecord for T {
            const V: PreDropRecord = PreDropRecord {
                entry_: if mem::needs_drop::<T>() {
                    Option::Some(drop_entry_::<T> as unsafe fn(*mut u8, *const ()))
                } else {
                    Option::None
                },
            };
        }
        &<T as HasRecord>::V
    }

    /// 带对象级 `PreDrop` 钩子的类型：记录按 `(T, Fin)` 一份。
    ///
    /// `Fin` 必须是零尺寸类型（函数项或非捕获闭包），见 [`zst_value_`] 的编译期断言。
    pub(crate) fn of_with<T: ?Sized, Fin: FnOnce(&mut T) + 'static>() -> &'static Self {
        trait HasRecord<Fin> {
            const V: PreDropRecord;
        }
        impl<T: ?Sized, Fin: FnOnce(&mut T) + 'static> HasRecord<Fin> for T {
            const V: PreDropRecord = PreDropRecord {
                entry_: Option::Some(pre_drop_entry_::<T, Fin> as unsafe fn(*mut u8, *const ())),
            };
        }
        &<T as HasRecord<Fin>>::V
    }

    /// 本登记是否什么都不做（既不需要 `Drop`，也没有钩子）。
    #[inline]
    pub(crate) fn is_noop_(&self) -> bool {
        self.entry_.is_none()
    }

    /// 执行清理：先跑 `PreDrop` 钩子，再 `drop_in_place`。
    ///
    /// # Safety
    ///
    /// - `base` 必须是当初分配 `T` 时那个 `StrongChunk<T>` 的首地址；
    /// - `meta` 必须是同一次分配登记下来的 `?Sized` 元数据（`Sized` 时为空）；
    /// - 数据必须尚未析构，且本登记只能用于它当初被创建时那个 `T`。
    pub(crate) unsafe fn run_(&self, base: *mut u8, meta: *const ()) {
        let Option::Some(entry) = self.entry_ else {
            return;
        };
        // SAFETY: 由调用方保证参数与类型匹配、且数据尚未析构
        unsafe { entry(base, meta) };
    }
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 单态化入口

/// 取出零尺寸钩子的唯一值。
///
/// 非捕获闭包与函数项都是零尺寸类型，只有一个值；零字节不承载任何位模式，因此"读未初始化
/// 内存"得到的仍是那个唯一值。捕获式闭包不是零尺寸，会被下面的 `const` 断言在编译期拒绝。
fn zst_value_<Fin>() -> Fin {
    const {
        assert!(
            mem::size_of::<Fin>() == 0,
            "PreDrop 钩子必须是非捕获闭包或函数项（零尺寸类型）"
        )
    };
    // SAFETY: 由上面的 const 断言保证 Fin 零尺寸，读取零字节是安全的
    unsafe { mem::MaybeUninit::<Fin>::uninit().assume_init() }
}

/// 单态化清理入口：由强块首址重建 `StrongChunk<T>`，再析构其数据区。
///
/// 数据区偏移由编译器在这个单态化里算出，因此记录无需保存数据地址。
///
/// # Safety
///
/// `base` 必须是 `StrongChunk<T>` 的首地址，`meta` 必须来自同一次分配，数据尚未析构。
unsafe fn drop_entry_<T: ?Sized>(base: *mut u8, meta: *const ()) {
    // SAFETY: 调用方保证 base/meta 来自同一块 StrongChunk<T>，且数据尚未析构
    let data = unsafe { data_ptr_of_::<T>(base, meta) };
    // SAFETY: 调用方保证数据尚未析构
    unsafe { ptr::drop_in_place(data) };
}

/// 单态化清理入口：先跑 `Fin`，再析构数据。
///
/// # Safety
///
/// 同 [`drop_entry_`]；此外钩子必须把值留在可析构的状态。
unsafe fn pre_drop_entry_<T: ?Sized, Fin: FnOnce(&mut T) + 'static>(
    base: *mut u8,
    meta: *const (),
) {
    // SAFETY: 调用方保证 base/meta 来自同一块 StrongChunk<T>，且数据尚未析构
    let data = unsafe { data_ptr_of_::<T>(base, meta) };
    let hook = zst_value_::<Fin>();
    // SAFETY: 调用方保证数据尚未析构
    hook(unsafe { &mut *data });
    // SAFETY: 钩子须把值留在可析构状态；调用方保证尚未析构
    unsafe { ptr::drop_in_place(data) };
}

// -- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ---- ----
// -- 类型级注册表

/// 类型级 `PreDrop` 钩子的注册表，挂在一个 `RootScope` 上，随整棵树回收。
///
/// 存在的理由：不能为了 `PreDrop` 逼使用者给每个类型实现一个 trait；只对自己关心的类型
/// 在这里登记一次即可，其余类型走"无钩子"的默认记录。
///
/// # 键的选择
///
/// 键是 [`core::any::type_name`]，**不是**"每类型一份常量的地址"：内容相同的常量会被编译器
/// 合并（例如 `u64` 与 `u32` 的无钩子记录），拿地址当键会让不同类型的钩子互相串味。
#[cfg(feature = "core-alloc")]
pub(crate) struct PreDropRegistry {
    /// 只登记被显式注册过的类型，典型数量很小，线性扫描即可。
    entries_: alloc::vec::Vec<PreDropEntry>,
}

#[cfg(feature = "core-alloc")]
struct PreDropEntry {
    /// 类型键，取自 [`core::any::type_name`]。
    key_: &'static str,
    /// 该类型的有效登记。
    record_: &'static PreDropRecord,
}

#[cfg(feature = "core-alloc")]
impl PreDropRegistry {
    /// 建一张空表（不分配）。
    pub(crate) const fn new_() -> Self {
        PreDropRegistry {
            entries_: alloc::vec::Vec::new(),
        }
    }

    /// 为类型 `T` 注册（或覆盖）一个类型级钩子。
    ///
    /// `hook` 本身只是用来推断 `Fin` 的类型，零尺寸、按值收下即丢弃。
    pub(crate) fn register_<T: ?Sized, Fin: FnOnce(&mut T) + 'static>(&mut self, _hook: Fin) {
        let key = core::any::type_name::<T>();
        let record = PreDropRecord::of_with::<T, Fin>();
        match self.entries_.iter_mut().find(|entry| entry.key_ == key) {
            Option::Some(entry) => entry.record_ = record,
            Option::None => self.entries_.push(PreDropEntry {
                key_: key,
                record_: record,
            }),
        }
    }

    /// 查找类型 `T` 的类型级登记。
    pub(crate) fn lookup_<T: ?Sized>(&self) -> Option<&'static PreDropRecord> {
        let key = core::any::type_name::<T>();
        self.entries_
            .iter()
            .find(|entry| entry.key_ == key)
            .map(|entry| entry.record_)
    }
}

/// 依"对象级 > 类型级 > 默认"选出类型 `T` 的有效登记。
///
/// 对象级与类型级都没有时退回 [`PreDropRecord::of::<T>`]；对 `!needs_drop` 的类型，那份记录
/// 的入口是空的，等价于"什么都不做"。
#[cfg(feature = "core-alloc")]
pub(crate) fn resolve_<T: ?Sized>(
    registry: Option<&PreDropRegistry>,
    object_level: Option<&'static PreDropRecord>,
) -> &'static PreDropRecord {
    if let Option::Some(record) = object_level {
        return record;
    }
    if let Option::Some(record) = registry.and_then(|registry| registry.lookup_::<T>()) {
        return record;
    }
    PreDropRecord::of::<T>()
}
