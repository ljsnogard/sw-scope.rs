# RootScope 多实例、显式 close 与 WeakPool 末槽释放

- 分支：`feat/multi-root-close`
- 起点：`4970fe9 Add 1KB-page multi-scope close stress test`
- 日期：`2026-10-29 01:00`
- 状态：**61 个库测试 + 3 个集成测试通过**；`cargo clippy --lib --tests` 干净

## 1. 已落地的技术决策

1. `RootScope` 不再全进程唯一，也没有隐式 static。调用方为每个 root 提供一个
   `&'static mut AtomicPtr<RootScope>` 发布槽位；每个 root 有自己的实体池、per-root
   类型表与分配器。
2. `RootScope` 不实现 `Drop`。`RootScope::close` 只设置 closing，让所有域拒绝新分配；
   真正的最终清理由最后一个完成清空的域线程负责。
3. Scope close 与 root close 职责分开：
   - `Scope::close`：析构本域数据、回收强池；弱池保留到对应 `Retain` 消失；
   - `RootScope::close`：树级结束信号；最终清理释放全部 WeakPool / ScopeInner /
     per-root 类型表 / root 存储。
4. 弱池的最后一个槽位由最后一个归还它的 `Retain` drop 负责摘链与释放；判定使用
   **原子活槽计数 + 池级锁 + 域弱池链锁**，不依赖非原子的 `alloc_count_`。

## 2. 公开 API

```rust
pub struct RootScope { /* 私有字段 */ }

impl RootScope {
    pub fn try_init_in<A>(slot: &'static mut AtomicPtr<RootScope>, page_size: usize, allocator: A)
        -> Result<&'static RootScope, &'static RootScope>;
    pub fn get(slot: &'static AtomicPtr<RootScope>) -> Option<&'static RootScope>;
    pub unsafe fn close(slot: &'static AtomicPtr<RootScope>);
    pub fn is_closing(&self) -> bool;
    pub fn slot(&self) -> &'static AtomicPtr<RootScope>;
}

impl Scope {
    pub fn new_in(root: &'static RootScope) -> Result<Self, ScopeError>;
    pub fn new_in_with_cfg<C>(root: &'static RootScope) -> Result<Self, ScopeError>;
    pub fn new_in_with_alloc<A, C>(root: &'static RootScope) -> Result<Self, ScopeError>;
    pub fn new_in_with_alloc_value<A, C>(root: &'static RootScope, alloc: A)
        -> Result<Self, ScopeError>;
}
```

`ScopeError` 新增 `ClosedRoot`。`Scope::new()` / `Scope::try_init_root()` /
`DEFAULT_ROOT_SCOPE` 全部删除；测试改为显式创建 root。

`RootScope::close` 的 unsafe 契约（也写在 rustdoc 与 README）：close 之后对由该 root
创建的 `Scope` / `Retain` / `Owning` / `Sharing` **只允许析构**，不得再读取、升级或放入；
最终清理之后任何句柄都会悬空。

## 3. root 回指与 per-root 类型表

查找链路改成：

```text
WeakSlot -> WeakPool.scope_ -> ScopeInner.root_ -> RootScope.metas_
```

- `ScopeInner.root_: Option<NonNull<RootScope>>`；`None` 只出现在不挂 root 的低层单元
  测试，以及 root 实体池构造完成之前。
- root 自己的实体池 `ScopeInner` 也调用 `set_root_(root)`，因此实体槽位（`ScopeEntity<A>`）
  的析构同样能找回 root。
- `alloc_::ensure_chunk_meta_` 改成 `ScopeInner::ensure_chunk_meta_`，从 `self.root_`
  走 root 的类型表。
- `index_::finish_destroy_` 用 `scope_root_of_slot_` 沿上述链路查表；查不到且 root 为
  `None` 时保守跳过（低层测试），否则触发 debug 断言。
- 所有 `RootScope::default_()` 调用点已删除。

`AnyTypeVTable` 仍是纯类型级函数指针，不携带 root；同一个 `ConstTypeId` 在不同 root 的
表里独立登记。

## 4. root 发布与生命周期

### 4.1 发布哨兵

`try_init_in` 用“槽位自身地址”作为初始化哨兵：

- 空 -> CAS 成哨兵 -> 分配 root -> 发布真实指针；
- 并发初始化者看到哨兵时自旋等待；
- 分配失败时把槽位换回空并 panic（root 是树的根，没有可恢复调用方）。

### 4.2 存储与分配器

```text
RootScopeStorage<A> { root: RootScope, alloc: A }
```

`RootScope.alloc_` 指向同一块存储尾部的具体分配器（共享形式）；最终清理时
`dealloc_storage_` 单态化函数把 `A` 从尾部移出，按原布局归还存储，最后析构 `A`。这样
不必把 root 分配器改成 `NonNull<dyn Allocator>`，也不必要求 root 保存分配器的所有权。

### 4.3 root 状态位：AtomicFlags

root 原本有三个独立的 `AtomicBool`：`closing`、`finalize_elected`、`finalizing`。它们都只在
root 级收尾路径上访问，而且“选举”本质上是对同一份状态做“检查 + 置位”，因此合并成
一个 `AtomicFlags<u32, AtomicU32>`：

```text
bit 0  CLOSING              已经收到 close 信号
bit 1  FINALIZE_ELECTED     某个线程已经赢得最终清理权
bit 2  FINALIZING           最终清理已经开始，新租约一律失败
```

- `is_closing` / `is_finalize_elected` / `is_finalizing` 都是对同一个字的原子读；
- `set_closing` / `set_finalizing` 用 `fetch_or`；
- `try_elect_root_finalize` 用一次 CAS 同时检查 `ELECTED | FINALIZING` 并置 `ELECTED`，
  因此“赢家唯一”不再依赖额外的 `AtomicBool`。

### 4.4 创建与 finalize 的串行化

- 创建新域在 `entity_pool_` 锁内检查 `closing_`、分配实体（实体身份槽位自动登记在实体池
  的弱槽位链上）、`live_scopes_ += 1`；
- `RootScope::close` 在同一把锁内置 `closing_`，因此不会与创建竞态；
- 实体不需要额外的链表 / 注册表：实体池的弱槽位链就是实体注册表。

### 4.5 最终清理顺序

由最后一个完成清空的域线程执行（或 `close` 发现 `live_scopes_ == 0` 时自己执行）：

1. 置 `finalizing_`，等 `active_`（root 使用租约计数）与 `flush_inflight_` 归零；
2. 把 root 实体池当普通域清盘：遍历实体池的弱槽位链，用实体 vtable 析构每个
   `ScopeEntity<A>`（锁内核 + 每域分配器）；
3. 回收 root 实体池的弱池 / 强池；
4. `drop` per-root `metas_`（必须在所有用户数据析构之后）；
5. 清空发布槽位；
6. `dealloc_storage_` 释放 root 存储。

## 5. Scope close / root close 与并发收尾

### 5.0 ScopeInner 的打包状态字

`ScopeInner` 不再有 `closed_` / `close_requested_` / `retired_` 三个独立 `AtomicBool`，
它们和句柄计数一起放进 `handles_: AtomicUsize`：

```text
最高位                         最低位
[RETIRED][CLOSE_REQUESTED][CLOSED][ 句柄计数 ... ]
```

- 低 `usize::BITS - 3` 位是 `Scope` 句柄计数；
- 最后一次 `decr_handle_` 用一次 CAS 同时完成“计数减一”与“置 `CLOSED`”，不存在
  “计数已归零、closed 仍为 false”的多字窗口；
- `request_close_` 用一次 `fetch_or` 同时置 `CLOSED | CLOSE_REQUESTED`；
- 退出登记用 `fetch_or(RETIRED)` 的返回值做 test-and-set，保证 `live_scopes_` 只减一次；
- 所有计数读取都必须 `& HANDLE_COUNT_MASK`，状态读取只检查对应位。

这样 `Scope` 的 clone / drop / close 都只碰同一个原子字，也少了两份字段与两次独立读。

### 5.1 Scope 的“清空”判定

`ScopeInner::try_retire_` 在域写锁下检查：

1. `handle_count_() == 0`；
2. 弱池链为空（所有槽位已归还，因此没有 `Retain`）；
3. 强池链为空（数据已析构、强池已回收）。

三者同时成立才把 `live_scopes_` 减一；`RETIRED` 位的 test-and-set 保证只减一次。若弱池链已空但
强池还在（正常析构路径不一定经过 `flush_data_`），先整链回收强池再判定。

### 5.2 两层租约（为什么不会在别的线程脚下释放实体）

**每实体使用租约** `ScopeInner.users_: AtomicUsize`：

- `Scope::drop` 在减句柄前取实体租约；
- `Retain::drop` 在弱计数归零、取到所属域之后立刻取实体租约；
- `try_retire_` 要求 `users_ == 1`（只有当前线程），否则把退出登记留给最后一个线程。

**每 root 使用租约** `RootScope.active_: AtomicUsize` + `flags_` 的 finalizing 位：

- `Scope::drop` / `Retain::drop` / `RootScope::close` 在触碰实体或 root 之前调用
  `RootLease::enter_`；`finalizing_` 已置真时申请失败，调用方不得再触碰树内内存；
- `finalize_` 先置 `finalizing_`，再等 `active_` 与 `flush_inflight_` 归零；
- `close` 只有在 `live_scopes_ == 0 && flush_inflight_ == 0 && active_ == 1`（只有自己）
  时才直接清理；若还有最后一个域线程持有租约，则让该线程赢得 `finalize_elected_` 后
  自行清理。

这样即使两个线程同时 drop 同一个域的最后两个 `Retain`，也不会出现“A 线程释放 root，
B 线程还拿着 `ScopeInner` 指针准备 flush”的窗口；`finalize_` 一定会等到 B 的租约归还。

### 5.3 finalize 选举

`close` 与“最后一个域线程”都可能观察到可以清理；`finalize_elected_` 的 CAS 保证只有
一个赢家：

- 域线程在 `try_retire_` 里把 `live_scopes_` 减到 0 且看到 closing 时参与选举，赢家
  在归还租约后 finalize；
- `close` 看到 `live_scopes_ == 0 && flush_inflight_ == 0` 时参与选举；它不等
  `active_ == 1`，因为并发 close / 正在退出的域线程都持有租约，等待会互相死锁；
- `finalize_` 先置 `finalizing_`，再等 `active_` 与 `flush_inflight_` 归零，因此其它
  租约持有者可以安全退出；
- 域线程若在 `try_retire_` 时看到 closing 仍为 false，会在归还租约前再调用
  `try_elect_finalize_` 补一次检查；`close` 先置 closing 再读 live 的顺序（两者都是
  `SeqCst`）保证不会出现双方都以为对方会清理的窗口。

## 6. WeakPool 末槽释放

### 6.1 现状审计（改造前）

- `WeakPool::alloc_count_` 是非原子 `PoolIndex`，只在域写锁内更新；
- `deallocate` 只被分配失败回滚使用，**没有**“对象死亡后归还槽位”的路径；
- `Retain::drop` 只减弱计数、可能析构数据，但从不归还槽位，因此弱池永远不会空；
- `WeakSlotState::reset_all` 用整体替换 `AtomicFlags` 的方式清状态，与清盘线程的原子读
  并发时构成数据竞争；
- 实体槽位在 `allocate_entity_` 里从未初始化为 `Created`，活槽计数会失真。

### 6.2 新结构

`WeakPool` 拆成三部分：

```text
capacity_ / cell_offset_                 只读
status_: UnsafeCell<WeakPoolStatus>      latest_free_ / pins_ / retire_pending_ / detached_
live_count_: AtomicU32                   活槽计数（末槽唯一判定）
locked_: AtomicBool                      池级自旋锁
prev_ / next_ / scope_                   由所属 ScopeInner 的弱池链锁保护
```

`ScopeInner` 新增 `weak_chain_: SpinningMutexOwned<WeakChain>`，把所有 `prev_` / `next_` /
链头 / 链尾的修改串行化。锁顺序固定为：

```text
域写锁 -> 弱池链锁 -> 池锁
```

链锁临界区只做指针操作，不调用用户代码。

### 6.3 归还路径

`Retain::drop` 在弱计数归零后：

1. 若槽位状态已是 `Destroyed`（自己刚析构或清盘已析构），调用
   `ScopeInner::release_slot_`；
2. `release_slot_` 在链锁内调用 `WeakPool::return_slot_`：
   - 池锁内把槽位推回空闲链，`live_count_.fetch_sub(1)`；
   - `fetch_sub` 返回 1 的那一次就是**唯一末槽**；
3. 末槽调用者继续在链锁内 `try_mark_detached_`：
   - 若 `pins_ == 0` 且 `live_count_ == 0`，标记 `detached_` 并摘链；
   - 若正被 flush 钉住，只置 `retire_pending_`，由最后一个 unpin 负责释放；
4. 摘链后在链锁之外用 `ScopeInner.allocator_` 释放池内存。

归还路径**不会**每次都重新跑 `flush_data_`：`release_slot_` 只返回“整条弱池链是否刚刚
清空”，调用方仅在 `chain_empty && handles == 0 && !RETIRED` 时才调用轻量的
`try_retire_only_`（拿写锁做退出登记 / 回收强池，不扫描池链）。这一点对性能很关键：
早期实现每个失效 `Retain` 都调用 `try_flush_`，会把“释放 N 个句柄”变成 O(N²) 的池链
扫描（见 §10）。

唯一性论证：

- 一个槽位只会由“亲眼看到弱计数从 1 减到 0”的那一个 `Retain` drop 归还；
- `live_count_` 的 `fetch_sub` 是原子读改写，只有返回值 1 的那次完成 1 -> 0；
- `detached_` 在池锁内检查并置位，保证同一个池只摘链 / 释放一次；
- 槽位在归还前一直计入活槽，因此 `WeakPool::of_slot_` 之后池不可能被别的线程释放。

### 6.4 与 flush 的并发

`flush_data_`：

1. 链锁内给链上每个池 `pin_`；
2. 释放链锁后遍历 / 析构；级联 `Retain` drop 只能把空池标记为 `retire_pending_`；
3. 链锁内逐个 `unpin_`，把钉住期间变空的池摘下，最后在锁外释放。

因此 flush 遍历中的池不会被释放；`WeakPool::return_slot_` 也不会在 flush 持有写锁时
重入域锁（它只拿链锁 + 池锁）。`try_flush_` 使用 `try_write`，级联路径抢不到锁就放弃，
不会自死锁。

### 6.5 与 root 最终清理的边界

`release_slot_` 只在 `ScopeInner` 仍存在时使用；root 最终清理要求 `live_scopes_ == 0`，
即所有域都已登记退出、弱池链为空，因此：

- root close **之后、最终清理之前**，`Retain` drop 仍会正常归还弱池；
- 最终清理不会走 `Retain` 归还路径，而是直接 `free_all_weak_pools_` 整链释放实体池；
- 最终清理之后不存在任何合法 `Retain`，再访问即违反 `close` 的 unsafe 契约。

## 7. 测试

新增：

- `independent_roots_do_not_share_type_tables`：两个 root / 两个槽位同时存在，各自登记类型、
  各自析构，close 后槽位都清空；
- `root_close_stops_puts_and_finalises_after_last_scope`：close 后放入返回 `ClosedRoot`，
  最后一个域退出后槽位清空；
- `root_close_waits_for_last_retain_then_finalises`：`Scope` 句柄先消失、`Retain` 后消失时，
  由最后一个 `Retain` 线程完成 root 清理；
- `root_close_rejects_new_scopes`：close 后不能再建域；
- `weak_pools_are_freed_when_last_retain_drops`：1KB 页、256 个对象跨多个弱池，逐个释放
  `Retain`，每个池地址恰好释放一次；之后同一个 Scope 仍可分配；
- `concurrent_last_retain_drops_free_pool_exactly_once`：8 个线程同时 drop 同一池的
  `Retain`，池只释放一次；
- `root_get_reflects_publication_state`、`root_entity_pool_creates_many_scopes`、
  `entity_vtable_drops_scope_allocator`：发布状态、实体池多域、实体 vtable 析构每域分配器；
- 1KB 页 / 12000 节点压力测试末尾追加 root close，验证跨线程清空后 root 被最终清理。

## 8. 仍然存在的泄漏 / unsafe 边界

1. **root 存储只在 close 后释放**：不调用 `RootScope::close` 时，root、实体池与身份槽位
   会一直存活（安全泄漏）。这是“显式生命周期”的设计选择。
2. **泄漏任何 `Scope` / `Retain` 会阻止 root 最终清理**：`live_scopes_` 不会归零，属于
   安全泄漏；调用方应先释放全部句柄再 close。
3. **实体内存按 root 实体池整池回收**：单个实体退出只释放自己的弱池 / 强池，实体 chunk
   所在的内存仍留在 root 实体池，直到 root 最终清理。`StrongPool` 仍是 bump-only。
4. **`mem::forget` 掉 `Owning` / `Sharing`** 会让对应数据永远无法析构，弱槽位也不会归还；
   这是 `Retain` 语义允许的安全泄漏。
5. **root close 的 unsafe 契约无法由编译器检查**：`Retain` 没有 `'root` 生命周期；调用方
   必须保证 close 后只析构、不再读取/升级。
6. **自旋锁不可重入**：弱池链锁 / 池锁的临界区不得调用用户代码；当前实现只做指针操作与
   分配器释放，满足该约束。
7. **`pre_drop` 测试的 release 崩溃已审查并修复**：根因是该测试仍按旧版
   `StrongChunk` 的“头部即整块”假设分配栈内存，并且身份 `WeakSlot` 在 `run_` 之前
   就失效；属于测试侧的过时假设 + 违反当前布局/生命周期不变量，不是生产代码回归。
   详见 §11；修复后 `cargo test --release` 全量通过。

## 9. “root 实体”与实体池的追踪方式

### 9.1 概念

“root 实体”不是 root 自己，也不是用户数据。它指的是一个 `Scope` 在 **root 实体池**里的
控制块：

```text
StrongChunk<ScopeEntity<A>> {
    StrongChunkBase,          // 类型 id / 身份弱槽位指针 / 池内偏移
    ScopeEntity<A> {
        ScopeLock<ScopeInner>, // 域写锁 + ScopeInner（弱池链 / 强池链 / root 回指）
        A,                     // 该域自己的池分配器
    },
}
```

用户放进 Scope 的 `T` 不在实体里：`T` 在域自己的 `StrongPool` 链上。实体只承载“域怎么
管理自己的池”以及每域分配器 `A` 的析构。

### 9.2 不再维护实体链表

早期实现额外维护了一条侵入式单链表：

```text
RootScope.entity_head_ -> ScopeInner(E1) --entity_next_--> ScopeInner(E2) -> ...
```

这是**多余的**。root 实体池本身就是一个 `ScopeInner`：它的弱池链 + 弱槽位状态已经能
枚举池中的每个 `StrongChunk`，而 `WeakSlot::strong_chunk()` 正好指回实体 chunk。
`ScopeEntity<A>` 只是 root 实体池里的“用户数据”，完全可以复用与普通域相同的
`flush_data_` 算法：

1. 钉住实体池的弱池链；
2. 遍历每个槽位：`Created` 的槽位用实体 vtable 析构 `ScopeEntity<A>`（锁内核 + 每域
   分配器），随后标记 `Destroyed`、断开强块指针；
3. 所有实体死亡后整链回收实体强池；
4. 解钉，最后 `free_all_weak_pools_` 释放实体身份弱池。

因此现在的 `ScopeInner` 不再有 `entity_next_` / `entity_slot_`，`RootScope` 不再有
`entity_head_ / entity_tail_`。实体池的弱槽位链就是唯一的“实体注册表”。

### 9.3 为什么之前会误以为需要链表

`StrongPool` 是 bump-only 的，块与块之间没有统一步长，`ScopeEntity<A>` 的布局随 `A`
变化，因此**不能**只靠强池内存反推“下一个实体 chunk 在哪里”。但弱槽位链从一开始就是
为此设计的：每个 `StrongChunk` 都有身份弱槽位，槽位的 `pool_order_` 能反推所属弱池，
弱池有兄弟链，所以“枚举所有实体”与“枚举所有用户对象”是同一个算法，不需要第二条链。

### 9.4 仍然需要 `live_scopes_`

`live_scopes_` 不用于枚举实体，只用于回答“是否所有域都已完成清空”。弱槽位在实体
生命周期内一直是 `Created`（root 实体池持有一份），无法单独表示“域已退出”；因此域的
退出仍由 `RETIRED` 位 + `live_scopes_` 计数。实体真正被析构要等 root close 的最终清盘，
此时 `live_scopes_ == 0`，不会再有域访问自己的 `ScopeInner`。

如果把实体析构提前到域退出时，`live_scopes_` 仍需要（用于 root close 的最终清理门槛），
但实体池的弱槽位可以在域退出时归还；当前选择“root close 时统一清盘”，与普通域的
`flush_data_` 语义保持一致，也避免在域退出路径上额外取得实体池的锁。

## 10. 性能：1KB 页 / 12000 节点压力测试

场景与 `src/stress_tests_.rs` 一致：4 个 1KB 页 Scope，每域 3000 个节点，共 12000 个；
节点按下标跨 4 个 Scope 成环；保留全部外部 `Retain` 后 4 线程并发 `close`，再释放所有
`Retain`。

环境：本机（Linux VM），`--release --offline`，release 取 5 轮中位数，debug 取 4 轮
中位数；单位 ms。不同机器/负载下绝对值会变，主要看相对关系。

| 阶段 | 基线 `4970fe9` release | 当前 release | 基线 debug | 当前 debug |
|---|---:|---:|---:|---:|
| create | 4.54 | 5.35 | 102.7 | 134.6 |
| close_total | 4.53 | 3.95 | 16.6–22.9 | 20.3 |
| close_max | 4.04 | 3.12 | 14.7–22.6 | 19.8 |
| release_dead_retains | 0.246 | 1.284 | 2.20 | 15.7 |

解读：

- **create** 慢约 18%（release）/ 31%（debug）：每次放入现在要拿域弱池链锁 + 池锁，
  还要维护活跃槽计数；基线没有这些同步。
- **close_total / close_max** 与基线同量级（release 略快，属于并发测量噪声）。
- **release_dead_retains**：当前约为基线的 5×（release）/ 7×（debug），但这是**语义
  差异**：基线释放失效 `Retain` 只做弱计数减一，槽位与弱池永远不回收；当前每个句柄
  要归还槽位、维护空闲链、必要时摘链并释放空池。1.28ms / 12000 ≈ 107ns 每个句柄，
  属于“确实做了回收工作”的成本。
- 修复前（同一份多 root 实现，但每个失效 `Retain` 都 `try_flush_`）的 release 阶段是
  **132ms**（约基线的 540×），并把 `cargo test --lib` 从 0.2s 拖到 2.3s；改成
  `chain_empty` 才 `try_retire_only_` 后回到 1.28ms。

因此当前性能结论：**创建路径有可解释的锁开销，close 与基线相当，失效句柄释放比基线慢
约 5× 但换来了真正的弱槽/弱池回收，且没有 O(N²) 退化。**

## 11. 审查：`pre_drop_runs_before_drop_exactly_once` 为什么在 release 崩溃

### 11.1 测试原本要证明什么

它是 `PreDropRecord` 语义的回归测试，目标有三条：

1. 对象级 `PreDrop` 钩子在数据 `Drop` **之前**执行；
2. 钩子与 `Drop` 都**恰好一次**；
3. 钩子执行时数据仍然有效（能看到 `"hello"`）。

同时它顺带覆盖 `PreDropRecord::of_with`（零尺寸钩子）与
`StrongChunk::init_with_value` / 类型擦除清理入口。

### 11.2 原实现的三处无效假设

**（a）把 `StrongChunk<T>` 的头部大小当成整块大小。**

原测试用：

```rust
let mut slot = MaybeUninit::<StrongChunk<Probe>>::uninit();
let base = make_probe_chunk_(&mut slot);
```

但当前 `StrongChunk<T, P>` 只是**头部视图**：

```text
size_of::<StrongChunk<Probe>>() = 24   // StrongChunkBase + PhantomData
layout_sized(Layout::new::<Probe>())    = 40 字节（头部 24 + Probe 16）
```

真实块布局是 `(StrongChunkBase, [meta 字], [predrop], data)`，由
`layout_sized` / `init_sized` 工厂决定。`init_with_value` 会把 `Probe("hello")`
写到偏移 24 处，也就是栈上 `slot` 之后的 16 字节——**越界写**。

debug 构建里这块栈内存后面通常还有填充/其它局部变量，写坏不立即致命；release
构建的栈布局不同，越界写会覆盖返回地址/保存寄存器，于是 SIGSEGV。这也解释了为什么
在测试里加一句 `eprintln!` 就能“修好”崩溃：它改变了栈布局，只是把 UB 藏起来。

**（b）身份 `WeakSlot` 在强块仍然存活时就被销毁。**

原 `make_probe_chunk_` 在函数内部创建：

```rust
let mut weak_slot = MaybeUninit::<WeakSlot<()>>::uninit();
let weak = &mut *weak_slot.as_mut_ptr();
WeakSlot::<()>::init_slots(slice::from_mut(weak), 1);
StrongChunk::init_with_value(chunk, NonNull::from(weak), Probe("hello"));
```

`init_with_value` 把 `weak` 的地址写进强块头部（`weak_chunk_`）。函数返回后
`weak_slot` 生命周期结束，强块里的身份指针变成悬垂指针。这违反了 `WeakSlot` 文档里的
核心不变量：

> 数据还活着 ⇒ 至少有一个 `Retain` 活着 ⇒ 这个 `WeakSlot` 还活着。

本测试的 `run_` 路径恰好不读 `weak_chunk_`，所以没有立即炸；但对象已经不是一个合法的
`StrongChunk`，任何 `is_data_alive()` / 类型擦除路径都可能 UAF。

**（c）对未初始化内存构造引用。**

`&mut *weak_slot.as_mut_ptr()` 与 `slice::from_mut(weak)` 在内存尚未初始化时就创建了
`&mut WeakSlot` / `&mut [WeakSlot]`，严格来说也是 UB；应该用
`WeakSlot::stack_empty_()` 或 `ptr::write` 先建立有效值。

### 11.3 结论：过时假设，而不是生产代码回归

- 起点提交 `4970fe9` 的独立 worktree 在 release 下同样 SIGSEGV，说明它与本轮多 root /
  close / 弱池改造无关；
- 它违反的是**当前** `StrongChunk` 的布局契约（头部视图 vs 工厂布局）和 `WeakSlot`
  生命周期不变量；
- `StrongChunk` 从早期“结构体自带数据区”演进成“头部视图 + `layout_*` / `init_*` 工厂”
  之后，这个测试的栈分配方式没有跟着迁移，属于代码演进遗留的过时假设。

### 11.4 修复

- 用 `ChunkBuf<8>`（`#[repr(C, align(16))]`，64 字节）作为真实块内存，并在测试里
  断言 `layout_sized(...).size() <= size_of::<ChunkBuf<8>>()`；
- 身份 `WeakSlot` 放到测试函数栈上（`WeakSlot::stack_empty_()` + 置 `Created`），
  活到 `run_` 结束之后；
- 去掉 `MaybeUninit<StrongChunk<T>>` 与对未初始化内存的引用；
- 测试注释补充“头部视图 / 真实布局 / WeakSlot 生命周期”三条契约。

修复后：

```text
cargo test --release --lib strong_::pre_drop_tests_   连续 10 轮通过
cargo test --release                                  64 + 3 + 3 全部通过
```
