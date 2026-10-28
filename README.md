# SW-Scope

使用三种智能指针来管理内存的 Arena。
其中两种是带有域生命周期的智能指针：
- `Owning<'a, T>` 独占指针，类似 `Box<T>`;
- `Sharing<'a, T>` 线程安全的引用计数，类似 `Arc<T>`;
还有一种是不带生命周期的 `Retain<T>`，它更像是一个 GC Handle，传递到哪里，生命周期便带到哪里。

## 使用思路

```rust
use core::{ptr, sync::atomic::AtomicPtr};
use std::alloc::Global;
use sw_scope::{Retain, RootScope, Scope};

async fn demo() {
    // 每个 root 都有自己的发布槽位；root 不再是进程级单例。
    let slot: &'static mut AtomicPtr<RootScope> =
        Box::leak(Box::new(AtomicPtr::new(ptr::null_mut())));
    let root = match RootScope::try_init_in(slot, 4096, Global) {
        Ok(root) | Err(root) => root,
    };

    let scope = Scope::new_in(root).expect("新建域");
    let retain = scope.put_async(42usize).await.expect("放入");

    {
        let mut x = retain.try_owning().expect("独占");
        *x = 58;
        drop(x); // `Owning` 析构只归还访问权，数据仍由 `Retain` 保活
    }

    // 显式结束域：析构域内数据、回收强池；弱池保留到 Retain 消失。
    scope.close();
    drop(retain);

    // root close 是更高一层的结束信号，标记为 unsafe：此后只能析构该 root 创建的
    // Scope / Retain，不得再读取或升级它们。最后一个清空的域线程负责最终清理。
    // SAFETY: 上面已经释放全部 Scope / Retain，之后不再访问它们。
    unsafe { RootScope::close(root.slot()) };
}
```

## 多 root、Scope close 与 root close

- `RootScope` 不再全局唯一、也不再默认 static。调用方为每个 root 提供
  `&'static mut AtomicPtr<RootScope>` 发布槽位；每个 root 有自己的实体池、类型表与
  分配器，同一个 `ConstTypeId` 在不同 root 里各自登记，不会串表。
- `Scope::close` 是**域级**结束：析构本域数据、回收强池；弱池与槽位保留到对应的
  `Retain` 全部释放，最后一个归还槽位的 `Retain` drop 负责把空 `WeakPool` 从
  `ScopeInner` 的弱池链上摘下并释放内存。
- `RootScope::close` 是**树级**结束信号：它只设置 closing，让所有域拒绝新分配；真正的
  最终清理由最后一个完成清空的域线程执行（实体 vtable 析构、root 实体池、per-root 类型
  表、root 存储）。
- root close 的 unsafe 契约：关闭之后只能继续析构该 root 创建的 `Scope` / `Retain`，
  不得再读取、升级或放入；最终清理完成后任何句柄都会悬空。

# sw-scope

`sw-scope` is an experimental Rust scope arena that explores a simple question:

> Can we combine Rust's compile-time lifetime checking with a small part of the idea behind garbage collection, without actually implementing a garbage collector?

The motivation comes from a class of objects that are very common in asynchronous systems, such as `CancellationToken`.

A cancellation state may be copied many times and then observed by futures running on different tasks, threads, or independent parts of a system. Those observers may all disappear at different times, and it may be impossible for any single owner to know when the underlying state is finally safe to destroy.

`Arc` solves the basic lifetime problem, but it represents ownership through reference counting. Once long-lived asynchronous objects start referring to one another, ownership cycles become possible, and reference counting alone cannot reclaim them.

A tracing GC can solve that problem, but it introduces a much larger runtime mechanism than many such applications actually need.

`sw-scope` explores a different division of responsibility.

## Two mechanisms, two jobs

First, use Rust's lifetime system for **temporary access**.

`Owning<'a, T>` and `Sharing<'a, T>` are borrowed access handles. They are derived from a `Retain<T>`, and their lifetimes are constrained by the borrow from which they came. This means the compiler can prevent these access handles from freely escaping into arbitrary long-lived ownership structures.

In other words:

```text
Retain<T>
    │
    ├── try_owning()  ──> Owning<'a, T>
    │
    └── try_sharing() ──> Sharing<'a, T>
```

`Retain<T>` may live independently and may even participate in reference cycles. The temporary `Owning` / `Sharing` access, however, remains subject to Rust's lifetime rules.

Second, use a small piece of the idea behind GC for **final cleanup**.

A `Scope` owns the storage for its objects. When the scope is explicitly ended, it can begin destroying the objects it owns without needing to inspect the references between those objects.

Therefore an object graph such as:

```text
A ───> B
↑      │
└──────┘
```

does not prevent cleanup.

The collector does not need to determine whether `A` or `B` is reachable from some root. It does not need to trace the object graph at all. The scope itself is the lifetime boundary: when the scope is collected, the objects belonging to that scope can be driven through their destruction state regardless of whether their internal references form trees, DAGs, or cycles.

This gives `sw-scope` a deliberately unusual combination:

```text
             Rust lifetime system
                    │
                    ▼
        temporary access is bounded
                    │
                    │
             Scope-owned storage
                    │
                    ▼
          explicit bulk destruction
                    │
                    ▼
       cycles do not block reclamation
```

The goal is therefore not to build a smaller GC.

The goal is to explore whether **compile-time lifetime checking can handle the parts that Rust is already good at, while scope-based collection can handle the part where reference counting becomes awkward**.

## Why is this worth experimenting with?

Because it sits between several familiar models:

```text
Box / Rc / Arc
    └── object lifetime follows ownership

Arena
    └── objects usually die together

Tracing GC
    └── reachability determines liveness

sw-scope
    ├── Scope owns the storage
    ├── Retain provides a long-lived handle
    ├── Owning / Sharing provide lifetime-bounded access
    └── Scope cleanup can destroy cyclic object graphs
```

The interesting question is whether this middle ground is useful in practice.

A successful implementation would be particularly interesting for systems containing large numbers of short-lived asynchronous state objects, where:

* references may be distributed across tasks or threads;
* reference relationships may form cycles;
* temporary access should remain checked by Rust's borrow system;
* explicit ownership of individual objects is inconvenient;
* and a larger lifetime domain can eventually be shut down as a whole.

`sw-scope` is therefore an experiment in combining **Rust's lifetime guarantees** with **a very small, deliberately non-tracing piece of GC-style reclamation**.

It may turn out that the additional machinery is not worth the complexity.

But if the model works, it could provide a useful alternative for asynchronous systems whose lifetime structure is too irregular for ordinary lexical scopes, while being too structured to justify a full garbage collector.
