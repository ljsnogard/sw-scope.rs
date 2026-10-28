# Scope::close 压力与并发耗时（1KB 页、12k 对象）

- 分支：`feat/root-strongchunk`
- 日期：`2026-10-28 22:45`
- 测试：`src/stress_tests_.rs`（`cargo test --lib` 单元测试）

## 场景

- 4 个 Scope，全部使用 `PageSize1KB`；
- 每个 Scope 放入 3000 个 `StressNode`，总计 12000 个；
- 每个节点带 `prev` / `next` 两个 `Retain`，按“同一下标跨 4 个 Scope 成环”连接，即
  `nodes[s][i].next = nodes[(s+1)%4][i]`；
- 保留全部外部 `Retain` 句柄（`nodes[s][i]`）后在 4 个线程里用 `Barrier` 同时调用
  `Scope::close`；
- 主线程用 `mpsc::recv_timeout(30s)` 检测卡死。

1KB 页下，每个 `StrongPool` 只能容纳约 20 个节点 chunk，每个 `WeakPool` 约 60 个槽位，
因此每个 Scope 都有上百个强池、几十个弱池，close 时必须遍历长池链。

保留外部 `Retain` 的作用：它保证 close 期间每个槽位弱计数非零，跨 Scope 的 `Retain`
释放不会在另一个 Scope 的关闭线程上触发级联析构；每个 Scope 只在自己的锁内清理自己的
槽位。这样测试测的是 close 本身的池链遍历与并发锁行为，而不是跨线程级联所有权。

## 测量结果（debug 构建，单次）

```text
scopes=4 per_scope=3000 total=12000
create≈100ms
close_total≈21ms
close_max≈20.7ms
release_dead_retains≈2.1ms
drops=12000
```

- 没有死锁；4 个 close 都在超时前返回；
- close 总耗时约 15ms，单线程最大约 15ms；
- 12000 个节点全部析构恰好一次；
- 释放失效外部 `Retain` 约 2.4ms，没有重复析构。

测试同时验证：只有 `Retain` 持有的对象由 `close` 清除；外部 `Retain` 变成失效句柄。

## 是否改成 close_async 或 spinlock

当前数据不支持立即改：

- `close` 是同步、CPU-bound 的池链遍历；本次 12k 节点约 15ms，没有 I/O 或长等待；
- `ScopeInner::try_flush_` 使用 `try_write`，不会阻塞等待锁，因此不会因为“持锁等别的
  Scope”而死锁；
- 每个 Scope 一把锁，4 个线程并发 close 没有出现锁竞争放大。

如果未来出现以下情况，再考虑 `close_async` / 自旋锁：

1. close 需要等待一个正在进行的 `put_async` 写锁（当前 `try_write` 失败会推迟到句柄
   析构时重试，不是阻塞等待）；
2. 单域对象数量级继续增长，close 达到几十/几百毫秒并阻塞异步执行器；
3. 需要在 close 期间让出执行权（`close_async`），或把锁的等待路径改成适应高竞争的自旋
   锁。

目前保持同步 `close` 与 `CooperativeRwLock`，避免为未测出的瓶颈增加 API 复杂度。
