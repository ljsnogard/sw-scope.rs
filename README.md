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
