# Root 强块池装载域实体（feat/root-strongchunk）

- 分支：`feat/root-strongchunk`（从 `feat/scope-lock` 派生）
- 日期：`2026-10-28 05:29`
- 状态：**可编译、38 个库测试通过的原型**

## 背景

上一轮把域实体放在 `ScopeEntity<A>` 的单块堆分配里，并用 `ScopesBpTree` 登记
`NonNull<ScopeInner>`。讨论中确认：

- “StrongChunk 是用户对象容器”不构成反对理由——RootScope 的用户就是各个 Scope；
- StrongPool 是字节 arena，每个 chunk 可以有不同的 `T` / 布局，足以容纳各种
  `ScopeInnerWithCfg`；
- 既然 `Scope` 已经持有直接指针，`ScopesBpTree` 只用于登记，确实可以去掉。

本轮按这个方向做了对照原型。

## 当前形态

```text
RootScope {
    entity_pool_: SpinMutex<ScopeInner>,          // 域实体的强块池
    metas_:       BPlusTree<usize, *const ()>,     // 用户对象的 ?Sized 元信息（未接锁）
    alloc_:       &'static (dyn Allocator + Send + Sync),
}
```

每个域实体是一个 `StrongChunk<ScopeEntity<A>, ()>`：

```text
StrongChunkBase | ScopeEntity<A> {
                      ScopeHeader {
                          lock_: CooperativeRwLock<ScopeInner, …>,
                          handles_, closed_, root_,
                          chunk_: NonNull<StrongChunkBase>,
                          weak_:  NonNull<WeakSlot<()>>,
                          drop_:  unsafe fn(NonNull<ScopeHeader>),
                      },
                      alloc_: A,
                  }
```

- 分配走 root 的 `entity_pool_`：
  `allocate_weak_()` 取身份槽位 → `allocate_(chunk_layout)` 取强块 →
  `StrongChunk::init_sized` → 在 chunk 数据区就地写 `ScopeEntity<A>`。
- `Scope` 仍然只是 `NonNull<ScopeHeader>`；头部首字段是锁，因此地址等价于
  `NonNull<RwLock<ScopeInner>>`。
- **没有 `ScopesBpTree`**：`Scope` 直接指针，不需要按 key 查树；root 不再维护登记表。
- **异质存储**：不同 `A` 类型的 `ScopeEntity` 在同一个 root 强块池里共存；新增测试
  `root_entity_pool_holds_heterogeneous_scope_entities` 验证了 `TaggedAlloc`（带状态）
  与 `Global` 两个域同时工作。
- **类型擦除析构**：`ScopeHeader.drop_` 保存单态化的
  `drop_scope_entity_::<A>`，root 侧不需要知道 `A` 就能析构；目前只保存，尚未在回收路径调用。
- 身份槽位在分配后标为 `Created`（弱计数 0），这样未来 root 拆除时可以用
  `WeakSlotState::for_each_live_slot_` 枚举所有域实体。

## 冲突域

- 域创建 / 实体分配只碰 `entity_pool_` 这一把 root 级锁；
- 域内 `put_async` 只碰各自实体的 `ScopeLock`，完全不经过 root；
- 没有 B+ 树锁，也没有树的查找 / 分裂 / 合并。

因此冲突域确实比上一版更小、更局部。剩余的单点只有实体池分配锁（未来可以把
`StrongPool` 的 bump 指针改成原子 CAS，进一步缩小）。

## 代价 / 仍需面对的硬问题

1. **StrongPool 只能 bump，不能单独回收 chunk。** 上一版的 B+ 树 `SlabList` 在 `remove`
   时会把槽位挂回空闲链，可以复用；StrongPool 的 `used_count_` 只增不减。也就是说：
   - 如果域实体是 root 生命周期、或创建次数很少，这个方案很好；
   - 如果 `Scope` 会频繁创建 / 销毁，实体内存只能等 root 拆除才回收，会持续增长。
   要支持逐实体回收，需要给 StrongPool 增加变长 chunk 的空闲链 / 合并，或换成可回收 slab。
2. **根级实体池仍是一把锁**；目前域创建会短暂串行化。可以把 bump 分配做成原子操作，
   或按页/线程分池。
3. **root 拆除枚举尚未实现**：弱槽位已标成 `Created`，但还没有遍历
   `entity_pool_` → 弱槽位 → chunk → `drop_` 的回收路径。
4. `metas_` 仍没有自己的锁（和上一版相同）。
5. 实体析构后 chunk / 弱槽位如何归还，取决于是选“不可回收 bump”还是“可回收 slab”。

## 测试

- `cargo test --lib`：**38 passed**
  - 新增 `root_entity_pool_holds_heterogeneous_scope_entities`：同一 root 下不同 `A`
    类型的实体共存；
  - 原有克隆 / 每域分配器 / 4 线程并发放入测试继续通过。
- `cargo check --lib` 四种 feature 组合通过；
- `cargo clippy --lib` 无新增告警；
- `format_use_imports.py --check` / `rustfmt --check` 通过。

## 两个分支的对比

| | `feat/scope-lock`（B+ 树登记） | `feat/root-strongchunk`（强块池） |
| --- | --- | --- |
| 实体存储 | root 分配器单块分配，树存 key | root 强块池中的一个 `StrongChunk` |
| 查找 | Scope 直接指针（树仅登记） | Scope 直接指针 |
| 异质 `A` | 支持（实体单块分配） | 支持（同一池中的不同 chunk） |
| 逐实体回收 | 树删除可复用数据槽 | **StrongPool 不可单块回收** |
| 冲突域 | 树锁（创建 / 删除） | 实体池锁（仅创建） |
| 类型擦除析构 | 尚未实现 | `ScopeHeader.drop_` 已预留 |
| 代码量 | 多一棵树 + comparer | 少一棵树，但多 chunk 头 / 弱槽位开销 |

## 建议

如果域实体基本是 root 生命周期（或创建频率远低于对象分配），`feat/root-strongchunk`
是更简单、冲突更小的形态；下一步只需补 root 拆除枚举与 `drop_` 调用。

如果 `Clone` + 引用计数意味着域会频繁创建 / 销毁，建议在这个分支上把实体池换成
**可回收的变长 slab**（或保留 B+ 树的 data arena 作为实体池），而不是继续用 bump-only
的 StrongPool；否则回收语义无法落地。

## new_with_cfg 创建路径（补充）

本轮把 `Scope::new_with_cfg<C>` 接到“实体存放在 root 强池”的路径上：

- `TrScopeInnerConfig` 增加 `type PoolAlloc: Allocator + Default + Send + Sync + 'static`；
  `DefaultConfig<A>` 的 `PoolAlloc = A`，因此配置类型真正决定每域分配器类型。
- `Scope::new_with_cfg<C>()` = `new_with_alloc::<C::PoolAlloc, C>()`：
  - `C::PageSize` 决定该域开新弱池 / 强池的页大小；
  - `C::PoolAlloc::default()` 作为每域分配器的值，内联在 `ScopeEntity<A>` 尾部；
  - 实体本身仍由 root 的 `entity_pool_` 以 `StrongChunk` 分配。
- `Scope::try_init_root<TyCfg>()` 现在用 `TyCfg::PageSize` 和 `TyCfg::PoolAlloc::default()`
  初始化默认 root 的实体池（已发布 root 不会被替换）。
- `Scope::new_with_alloc` / `new_with_alloc_value` 保留，作为显式指定分配器类型 / 值的入口。

新增测试 `new_with_cfg_creates_scopes_across_root_entity_pools`：连续创建 300 个域，
验证 root 的实体强池会按需开新池、句柄地址互不相同。

**仍未接入**：`C::LockAlloc` 尚未用于锁内核；固定锁类型使用 root 的分配器。
这一点已在 `TrScopeInnerConfig` 的文档中标注 TODO。
