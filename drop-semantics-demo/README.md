# drop-semantics-demo

这是一个**对照实验**，用来实证 `dev-notes/strong-20260921-2150.md` §8 的结论：
一个"不在清盘时替对象负责"的 arena（这里用 `bumpalo::Bump`）会在若干**普通写法**下
真实泄漏对象持有的堆内存。

## 它为什么是独立 crate

`sw-scope` 本体保持**零外部依赖**（`cargo tree -p sw-scope` 只有它自己一行）。本 crate
单独声明 `bumpalo`，因此：

- 在 `sw-scope/` 下跑 `cargo test` **不会编译它**（实测：只 `Compiling sw-scope`），
  更不会给 sw-scope 引入任何依赖；
- 只在本目录下单独运行，或从 sw-scope 根目录用 `-p` 指定。

## 运行

```text
# 方式一：进入本目录
cd sw-scope/drop-semantics-demo
cargo run

# 方式二：从 sw-scope 根目录指定包
cargo run -p sw-scope-drop-semantics-demo
```

程序会打印一张对照表，并对每条结论做断言；断言全部通过后退出码为 0，否则 panic。

## 测量方式

两条独立证据，避免"只是析构函数没打印"这类误判：

1. **进程级分配器计数器**：`#[global_allocator]` 包一层 `GlobalAlloc`，统计各场景的
   `alloc` / `dealloc` 字节数。判据是"对象里的 payload（64 KiB）是否被归还"，
   而不看净值为 0——因为 arena 自身的 chunk 本来就要占内存（约 496 字节）。
2. **带 `Drop` 的探针类型**（`HeapThing`）：直接数析构次数。

## 观察到的结果

| 场景 | alloc | dealloc | 净泄漏 | drops | 结论 |
| --- | --- | --- | --- | --- | --- |
| 基线：普通作用域里的值 | 65536 | 65536 | 0 | 1 | 仪器有效 |
| §8.1 匿名临时值（`bump.alloc(..);`） | 66032 | 496 | **65536** | **0** | 泄漏 1 个 payload |
| §8.4 引用丢失（句柄离开作用域） | 66032 | 496 | **65536** | **0** | 泄漏 1 个 payload |
| §8.5 互相引用的环 | 131568 | 496 | **131072** | **0** | 泄漏 2 个 payload |
| 对照：`bumpalo::boxed::Box` | 66032 | 66032 | 0 | 1 | 不泄漏 |

## 结论

- `bumpalo::Bump` 的文档明确写着被 bump 分配的值**永远不会**运行 `Drop`；表中前三行就是
  这句话的实测形态：payload 从未归还、析构次数为 0。
- 同一个 arena、同一个 payload，只把"谁负责析构"从"没人"换成"句柄"（`boxed::Box`），
  净泄漏立刻变成 0。**所以问题不在 arena 实现，而在"没有任何一处代码仍然知道 `T`
  并愿意替它析构"**。
- sw-scope 的 `Retain` 是 GC Handle 式的长期句柄，允许对象先失去所有引用、再等 Scope
  清盘；`mem::forget` 句柄与 `Sharing` 环这两种写法更是**没有任何句柄会活下来**。
  因此 sw-scope 必须保留一条不依赖句柄、且能在编译期类型已擦除的前提下工作的清盘析构
  路径——这就是 §7/§8 与 §4 那条"按类型去重的析构登记"存在的全部理由。

## 说明

- 这里的"匿名临时值"对应 sw-scope 的 `scope.put(|| Thing::new());`（返回的 `Retain`
  当场析构）；bumpalo 没有 `Retain` 这种长期句柄，用"引用被丢弃"来等价表达。
