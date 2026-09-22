# SW-Scope

使用三种智能指针来管理内存的 Arena。
其中两种是带有域生命周期的智能指针：
- `Owning<'a, T>` 独占指针，类似 `Box<T>`;
- `Sharing<'a, T>` 线程安全的引用计数，类似 `Arc<T>`;
还有一种是不带生命周期的 `Retain<T>`，它更像是一个 GC Handle，传递到哪里，生命周期便带到哪里。

## 使用思路

```rust
fn demo() {
    fn owning_somewhere(retain: Retain<usize>) -> Retain<usize> {
        let mut x = retain.try_owning().unwrap();
        *x = 58;
        retain
    }

    fn share_everywhere(retain: Retain<usize>) -> Retain<usize> {
        // 依然可以成功尽管强引用一度归零，但 retain 还在，scope 也在
        let x = retain.try_sharing().unwrap();
        assert_eq!(*x, 58);

        // 手动制造一个泄露，下次 try_owning 就会失败
        let _ = Box::leak(Box::new(x));
        retain
    }

    let mut scope = Scope::new();
    let retain = scope.put(42);
    {
        let x = retain.try_owning().unwrap();
        assert_eq!(*x, 42);
    }
    let retain = owning_somewhere(retain);
    {
        let x = retain.try_owning().unwrap();
        assert_eq!(*x, 58);
    }
    let retain = share_everywhere(retain);

    assert!(retain.try_owning().is_none());
    assert!(retain.try_sharing().is_some());
}
```

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
