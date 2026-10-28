# ScopeLock 改为同步 SpinningMutex（feat/spin-scope-lock）

- 分支：`feat/spin-scope-lock`（从 `main` 切出）
- 日期：`2026-10-28 22:52`
- 状态：**52 个库测试 + 3 个集成测试通过**

## 1. 锁替换

```text
旧：ScopeLock = CooperativeRwLock<ScopeInner, usize, AtomicUsize, LocksOrderings, &'static dyn Allocator>
新：ScopeLock = SpinningMutexOwned<ScopeInner>
```

- `ScopeEntity<A> { lock_, alloc_ }` 不变，但锁不再需要锁内分配器；
- `ScopeInner.lock_` 仍指回包含自身的锁；
- `Scope::with_inner_` / `ScopeInner::try_flush_` 都是
  `lock_session().lock().wait()` 同步自旋；
- `try_flush_` 在释放路径上会等待锁，而不是 `try_write` 的非阻塞失败。

## 2. 异步 API 全部改为同步

`TrScope` 删除了所有 GAT + `TrMayCancel` future：

```rust
unsafe fn emplace_with_predrop<M, P>(&self, layout, emplace, predrop)
    -> Result<Retain<M::Target>, Self::Err>;
unsafe fn emplace<M>(&self, layout, emplace) -> Result<Retain<M::Target>, Self::Err>;
fn put_with_factory_predrop<Ft, Fp, P, T>(&self, val_factory, predrop_factory)
    -> Result<Retain<T>, Self::Err>;
fn put_str_with_predrop<P>(&self, str, predrop) -> Result<Retain<ScopeStr>, Self::Err>;
fn alloc_uninit_slice<P, T>(&self, length, predrop)
    -> Result<Retain<[MaybeUninit<T>]>, Self::Err>;
```

`Scope` 的入口：

```rust
pub fn put<T>(&self, data: T) -> Result<Retain<T>, ScopeError>;
```

- 删除 `Scope::put_async`、`scope_*_async`、`gen_may_cancel_future` 包装；
- 删除 `TrScopeExt`；
- 单元测试、demo、链表集成测试、stress 测试全部改为同步调用；
- `demo` / 链表测试不再需要 compio runtime。

## 3. 压力测试对比

同一 `stress_tests_` 场景：4 个 1KB 页 Scope × 3000 节点 = 12000，跨域循环引用，
4 线程并发 close。

旧（CooperativeRwLock + async put，debug，单次样本）：

```text
create ≈ 88–101ms
close_total ≈ 15.6–21ms
```

新（SpinningMutex + sync put，debug，5 次运行）：

```text
create      ≈ 66.1–68.3ms
close_total ≈ 16.0–17.8ms
close_max   ≈ 14.8–15.9ms
release_dead_retains ≈ 2.1–2.2ms
```

结论：

- 没有死锁；12000 个节点仍恰好析构一次；
- close 耗时与旧锁在同一量级（约 16–18ms），没有明显退化；
- 创建阶段明显变快（去掉 async future / runtime 开销）；
- 当前不需要 `close_async`：close 是 CPU-bound 池链遍历，没有等待 I/O；
- 自旋锁的等待只发生在真正冲突时；本测试中 4 个 Scope 各自独立，锁竞争很低。

## 4. 注意

- `try_flush_` 现在是阻塞自旋等待；不能在已经持有同一 Scope 锁的路径上重入。
  当前重试入口来自 `Retain` / `Owning` / `Sharing` 的释放路径，不持有 Scope 锁。
- `SpinningMutex` 没有 async 取消路径；原 `ScopeError::ClosedScope` 对锁错误的映射保留，
  仅作为异常状态的兜底。
- 外部 `Retain` 仍可在 close 后安全 drop；失效句柄升级返回 `None`。
