# RootScope 类型信息表与擦除析构（feat/root-strongchunk）

- 分支：`feat/root-strongchunk`
- 起点：`096758a Add AnyTypeVTable`（方向正确但未编译完）
- 日期：`2026-10-28 18:56`
- 状态：**可编译、50 个库测试通过**；类型擦除析构路径已接通

## 1. 为什么类型信息归 RootScope

强块需要在不携带 `T` / `P` 泛型的情况下完成析构，而 `T` / `P` 的组合在运行期
只有 `ConstTypeId` 可用。整棵 Scope 树里所有强块都由同一个 root 管理，root 是天然
的类型级注册中心，因此把 vtable 放在 `RootScope::metas_`：

```text
RootScope {
    entity_pool_: SpinningMutexOwned<ScopeInner>,
    metas_:       SpinningMutexOwned<BPlusTree<ConstTypeId, AnyTypeVTable>>,
    alloc_:       &'static (dyn Allocator + Send + Sync),
}
```

`metas_` 自带自旋锁。查询接口一律返回 `AnyTypeVTable` 的**副本**，锁在返回前释放；
因此析构路径拿到 vtable 后可以安全地先释放锁、再执行用户 `Drop`。

## 2. `ConstTypeId` 存放在哪里

`StrongChunkBase` 增加 `type_id_: ConstTypeId`，正好占用旧 `drop_` 指针的位置：

```text
refc_(4) + pool_order_(2) + dat_offset_(2) + weak_chunk_(8) + type_id_(8) = 24 字节
```

头部仍是三个指针宽度；Sized 数据偏移仍为 3 个指针，`?Sized` 仍为 4 个指针。
强块不再保存析构函数指针，也不保存 root 指针。

## 3. `?Sized` 的每对象 metadata 归属

类型表的一条记录无法承载每个切片的长度，因此：

- **每对象 metadata 直接写进强块的 meta 槽**（一个指针宽度），不再指向 root 树里的
  另一个指针字；
- `AnyTypeVTable` **删除了 `meta: *const ()` 字段**：保留一个语义含糊的类型级指针只会
  诱导错误使用；
- 强块的 `meta_raw_()` 直接读该槽，`pointee_meta_()` 返回这个裸字，重建胖指针时交给
  `meta_from_raw_`。

强块布局仍是：

```text
(StrongChunkBase, [meta 字], [predrop 对象], data)
```

## 4. Predrop 的擦除方案

只按 `ConstTypeId::of::<T>()` 注册时 vtable 不知道 `P`。因此改用合成键：

```rust
struct ChunkTy<T: ?Sized, P>(PhantomData<(*const T, P)>);
```

- `StrongChunk<T, P>::type_id_()` 返回 `ConstTypeId::of::<ChunkTy<T, P>>()`；
- `RootScope::ensure_chunk_meta_::<T, P>()` 用**同一个**函数生成键；
- vtable 的析构入口 `drop_chunk_entry_::<T, P>` 同时单态化 `T` 与 `P`；
- 同一个 `T` 配不同 `P` 会得到不同条目，因此 `P::pre_drop` 不会串味。

`drop_in_place` 的签名定为：

```rust
type FnDropInPlace = unsafe fn(*mut (), *const ());
```

参数是**数据指针**（thin）与每对象 metadata 裸字，直接 `ptr::drop_in_place::<T>`；
它不读取 `StrongChunk` 头部，因此用户数据可以先于强块析构，也可以在 chunk 头已不可用时
单独调用。Predrop 拆成独立入口：

```rust
type FnPreDrop = unsafe fn(*mut ());
```

它只接收强块首址，用于定位并消费内联存储的 `P` 对象。数据析构不依赖它；只有 Predrop
对象的位置本身与 chunk 布局绑定。

## 5. 单例假设与锁顺序

- `RootScope` 全进程唯一，固定发布在 `DEFAULT_ROOT_SCOPE`；
- `try_init_default_root_scope_` 不再接收自定义 `AtomicPtr`，只能初始化默认 root；
- 类型信息的所有注册 / 查找都走 `RootScope::default_()`；
- `Scope`、`ScopeEntity`、`WeakPool`、`WeakSlot` 都不保存 root 指针；
- `try_init_default_root_scope_` 在初始化中写入哨兵值（发布点自身地址）；
  `default_()` 会自旋等待哨兵被真实 root 替换；未初始化时返回 `None`。

锁顺序：

1. 放入：`ScopeLock` → `metas_` 锁（登记 vtable），不接触 `entity_pool_`；
2. 创建域：先 `entity_pool_` 分配，锁释放后才登记实体 vtable（两者不嵌套）；
3. 析构：先用弱槽位状态 CAS 认领 → 读强块 `type_id_` → `metas_` 锁取副本并释放 →
   执行 `P::pre_drop` / `T::drop`。用户代码执行时 `metas_` 锁已释放。

低层 `ScopeInner` 单元测试可能没有初始化 root；此时登记与析构都保守跳过（测试里用
`mem::forget` 避免进入该路径）。生产路径上 root 必然存在，debug 构建下“root 已存在但
查不到 vtable”会触发断言。

## 6. 接线位置

- `StrongChunk::init_with` 写 `type_id_`、meta 裸字与 Predrop 对象；
- `ScopeInner::put_value_with_` / `emplace_value_with_` 在分配成功后调
  `RootScope::ensure_chunk_meta_::<T, P>()`；
- `ScopeEntity::allocate_entity_` 为 `ScopeEntity<A>` 登记 `P = ()` 的 vtable；
- `index_::claim_and_destroy_if_unreachable_` 按 `type_id_` 查表：先调
  `run_pre_drop_`（如有），再用 chunk 的 `data_addr_()` / `pointee_meta_()` 调
  `run_drop_`，保持 `try_claim_destroy` 的“恰好一次”语义；数据析构本身不接收 chunk 地址；
- `scope_emplace_with_predrop_async` 不再是占位返回，已接到锁内的
  `emplace_value_with_`，因此 `?Sized` / Predrop 的公开异步入口可用。

## 7. 测试

`cargo test --lib`：**50 passed**。本轮新增：

- `ensure_meta_registers_once_per_type_id`：同一 id 的工厂只执行一次，`get_meta_` 返回同一条目；
- `vtable_drop_only_needs_data_pointer`：不构造 `StrongChunk`，仅凭数据指针就能完成类型级
  析构，验证 vtable 与 chunk 地址解耦；
- `chunk_type_id_includes_predrop_type`：`(T, P)` 合成键能区分 Predrop 类型；
- `repeated_puts_of_same_type_drop_each_once`：同类型多个对象各析构一次；
- `unsized_slice_drop_reads_per_object_metadata`：`?Sized` 切片按 per-object 长度逐元素析构；
- `fixed_sized_aligned_drop_runs_once`：非 ZST、16 字节对齐、内嵌 `String` 堆字段的固定尺寸
  类型恰好析构一次；
- `trait_object_dst_drop_uses_vtable_metadata`：`dyn Trait` 的 vtable metadata 能还原并触发
  具体类型析构；
- `predrop_vtable_runs_hook_then_drop`：Predrop 先于 Drop 且各一次；
- `put_str_keeps_length_metadata`：`str` 长度 metadata 经类型擦除路径还原为 `&str`；
- `retain_drop_runs_typed_destructor` 现在显式断言 `DropProbe` 是 ZST；
- 原 `retain_drop_runs_typed_destructor` / `owning_and_sharing_handles_roundtrip` /
  `new_with_cfg_creates_scopes_across_root_entity_pools` / demo 均恢复为真实析构，不再
  `mem::forget`。

强块布局测试已改为检查 meta 槽里的裸字，并新增 `ConstTypeId` 占一个指针宽度的断言。

## 8. 仍未决 / 未做

1. `tests/` 下三个集成测试仍按旧的同步 API（`Scope::put` / `put_str` / `put_with`、
   父子域）编写，在本轮开始前就无法编译；本轮的验收范围是 `cargo test --lib`，这些文件
   需要等同步 API / 父子域迁移完成后再重写。
2. StrongPool 仍是 bump-only，实体 chunk 与用户 chunk 的逐块回收 / 复用尚未实现。
3. `try_put*`、`set_pre_drop` 等同步入口仍是 `todo!()`；显式生命周期边界已改为
   `Scope::close`，`ScopeHeader` 已移除，详见 `scope-close-20261028-2037.md`。
4. `AnyTypeVTable.display` / `debug` 字段尚未接入，`display` / `debug` 入口的签名还没有
   为 `?Sized` 加上 metadata 参数。
5. 自定义 root 不再支持；若以后需要，必须先重新讨论单例假设与类型表归属。
