# SW-Scope

`sw-scope` 是一个实验性的 Rust scope arena。

它想探索的问题是：

> 能否用 Rust 的生命周期检查处理“临时访问”，
> 再用一小块类似 GC 的“批量清盘”思想处理引用计数不擅长的清理问题，
> 而不实现一个真正的 tracing GC？

这里的对象由三种智能指针访问：

- `Retain<T, M = Local>`：类似 GC Handle，不带生命周期，可以独立传递，也可以形成引用环；
- `Owning<'a, T, M = Local>`：独占访问，类似 `Box<T>`；
- `Sharing<'a, T, M = Local>`：引用计数共享访问，类似 `Arc<T>`。

`Owning` / `Sharing` 的 `'a` 来自产生它们的 `Retain` 借用，因此它们不能逃逸成任意长生命周期的所有权结构。

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

## 类型模型

```rust
pub struct Local;
pub struct Shared;

pub struct Scope<const CELL_SIZE: usize = DEFAULT_CELL_SIZE, M = Local> {
    // ...
}

pub type LocalScope<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> =
    Scope<CELL_SIZE, Local>;

pub type SharedScope<const CELL_SIZE: usize = DEFAULT_CELL_SIZE> =
    Scope<CELL_SIZE, Shared>;

pub struct Retain<T, M = Local> { /* ... */ }
pub struct Owning<'a, T, M = Local> { /* ... */ }
pub struct Sharing<'a, T, M = Local> { /* ... */ }
```

- `Local`：当前唯一实现的模式，单线程使用；
- `Shared`：预留的跨线程模式，当前没有可用构造入口；
- 现有 `Scope<CELL_SIZE>` / `Retain<T>` / `Owning<'a, T>` / `Sharing<'a, T>` 默认仍等价于 Local 模式。

内部 root：

```rust
pub(crate) struct RootScope<A, const CELL_SIZE: usize, M = Local> {
    // root 自己的域节点、共享扩展、分配器、线程模式 marker
}
```

`RootScope` 只负责：

- 持有整棵树的强池 / 弱池 / `PreDrop` 注册表；
- 作为域树的根节点，供 `Scope::new()` 创建顶层子 Scope；
- 提供反向定位 root 的能力；
- 通过 `Scope::try_config_root(...)` 初始化，但绝不以 `Scope` 形式返回。

它本身不是用户可持有的 Arena。

## 使用思路

```rust
use sw_scope::{LocalScope, Retain, Scope, TrScope};

fn owning_somewhere(retain: Retain<usize>) -> Retain<usize> {
    let mut x = retain.try_owning().unwrap();
    *x = 58;
    // Owning 实现 Drop，借用持续到它被析构
    drop(x);
    retain
}

fn share_everywhere(retain: Retain<usize>) -> Retain<usize> {
    // 即使强引用一度归零，只要 Retain 还在，数据仍可重新共享
    let x = retain.try_sharing().unwrap();
    assert_eq!(*x, 58);

    // 手动制造一个泄漏：下次 try_owning 就会失败
    let _ = Box::leak(Box::new(x));
    retain
}

fn demo() {
    // `Scope::new()` 返回的是 RootScope 的子 Scope，而不是 RootScope 自身。
    let mut scope = LocalScope::new();
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

因此对象之间有环：

```text
A ───> B
↑      │
└──────┘
```

不会阻止清盘。清盘不需要判断对象图可达性，也不需要 tracing。

## 为什么值得实验

`sw-scope` 位于几种熟悉模型之间：

```text
Box / Rc / Arc
    └── 生命周期跟随所有权

Arena
    └── 对象通常一起死亡

Tracing GC
    └── 可达性决定存活

sw-scope
    ├── Scope 拥有存储
    ├── Retain 提供长生命周期句柄
    ├── Owning / Sharing 提供生命周期受控的访问
    └── Scope 清盘可以销毁成环对象图
```

## 路线图

1. **单线程正确性**
   - RootScope 去用户化；
   - 去除 RootShared 式的全局树状态；
   - `Scope` / 各类句柄保持 `!Send + !Sync`；
   - 完善 `try_clone` / `try_alloc_slice_uninit`、环与 `mem::forget` 的清盘测试。
2. **Local 模式收口**
   - 默认 root 改为线程本地或显式 local root；
   - 明确 LocalScope 的“单线程树”边界；
   - 保持 `Retain<T, Local>` / `Owning<'a, T, Local>` / `Sharing<'a, T, Local>` 的
     `!Send + !Sync` 语义。
3. **SharedScope**
   - 为三种句柄补 `M = Shared` 的实现；
   - 插入数据要求 `T: Send + Sync`；
   - 每父域子链锁；
   - root 弱池、`PreDrop` 注册表、teardown task 的锁设计；
   - 再补 `SharedScope` 与 `M = Shared` 句柄的 `Send` / `Sync` marker。

当前阶段先完成第 1 步；第 2、3 步都还在类型占位和设计阶段。
