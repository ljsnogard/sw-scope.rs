# SW-Scope

`sw-scope` 是一个实验性的 Rust scope arena。

它想探索的问题是：

> 能否用 Rust 的生命周期检查处理“临时访问”，
> 再用一小块类似 GC 的“批量清盘”思想处理引用计数不擅长的清理问题，
> 而不实现一个真正的 tracing GC？

这里的对象由三种智能指针访问：

- `Retain<T, M = Local>`：类似 GC Handle，不带生命周期，可以独立传递，也可以形成引用环；
- `Owning<'a, T, M = Local>`：独占访问，类似 `Box<T>`，但生命周期保证它不会被长期持有；
- `Sharing<'a, T, M = Local>`：引用计数共享访问，类似 `Arc<T>`，同样地，生命周期保证它只能临时共享。

`Owning` / `Sharing` 的 `'a` 来自产生它们的 `Retain` 借用，因此它们不能逃逸成任意长生命周期的所有权结构。
因此 Scope 清盘是完全有可能在尊重“独占”和“分享”的语义前提下，可以不扫描对象可达性，也能清理潜在的循环引用。

## 当前阶段

当前只实现**单线程模式**：

```text
Root / Scope / Retain / Owning / Sharing
    => !Send + !Sync
```

原因是先把树结构、清盘顺序、PreDrop、弱槽位生命周期在单线程下做正确。
线程安全 / 跨线程 Scope 只保留类型层面的扩展位，不在这一阶段实现。

因此：

- `Retain<T, Local>` / `Owning<'a, T, Local>` / `Sharing<'a, T, Local>` 现在都不能跨线程发送；
- `Scope<..., Local>` 现在也不能跨线程发送或共享；
- `RootScope` 是内部管理员，永远不会作为 `Scope` 返回给用户；
- RootScope 的初始化是必须的，通过 `Scope::try_config_root(...)` 完成，但它只返回是否初始化成功；
- 用户拿到的 `Scope` 永远是 root 的子 Scope。

这与 gc-arena 的选择类似：先把整个 arena/GC 对象图限制在单线程里，
因此无需给 `T` 增加 `Send` / `Sync` 约束。

## 示例

```rust
use sw_scope::{Retain, Scope, TrScope};

fn reown_somewhere(retain: Retain<usize>) -> Retain<usize> {
    let mut x = retain.try_owning().unwrap();
    *x = 58;
    // 我们故意不 drop 来展示 Retain 的作用
    // drop(x);
    retain.clone()
}

fn share_everywhere(retain: Retain<usize>) -> Retain<usize> {
    // 依然可以成功，尽管强引用一度归零，但 retain 还在，scope 也在
    let x = retain.try_sharing().unwrap();
    assert_eq!(*x, 58);

    // 手动制造一个泄露，下次 try_owning 就会失败
    let _ = Box::leak(Box::new(x));
    retain
}

let mut scope = Scope::new_local();
// 可以直接当做普通对象池
let x = Owning::try_new_local(42, &mut scope).unwrap();
// 从这里开始 Owning 语义会让位于 Retain 语义，但依然是确定性析构的。
let retain = x.retained();
drop(x);
{
    let x = retain.try_owning().unwrap();
    assert_eq!(*x, 42);
}
let retain = reown_somewhere(retain);
{
    let x = retain.try_owning().unwrap();
    assert_eq!(*x, 58);
}
// 可以从一个独占访问升级为共享，这可能是比较有意思的地方。
let retain = share_everywhere(retain);

assert!(retain.try_owning().is_err());
let shared = retain.try_sharing().unwrap();
assert_eq!(*shared, 58);
```

## 两种机制

### 1. Rust 生命周期负责“临时访问”

```text
Retain<T, M>
    │
    ├── try_owning()  ──> Owning<'a, T, M>
    │
    └── try_sharing() ──> Sharing<'a, T, M>
```

`Retain<T, M>` 可以独立存续，也可以参与引用环；
`Owning` / `Sharing` 则受 `&Retain` 借用生命周期约束，不会随意逃逸。
当前 `M = Local` 是唯一实现。

### 2. Scope 负责“批量清盘”

`Scope` 拥有对象的存储。显式清盘时，它会把本域中仍然存活的对象逐个：

```text
PreDrop 钩子
    -> drop_in_place
    -> 归还弱槽位
    -> 回收强池
```

因此对象之间有环，不会阻止清盘。清盘不需要判断对象图可达性，也不需要 tracing。
