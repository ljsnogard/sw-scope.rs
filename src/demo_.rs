use std::boxed::Box;

#[cfg(test)]
use crate::*;

#[cfg(test)]
#[test]
fn demo() {
    fn owning_somewhere(retain: Retain<usize>) -> Retain<usize> {
        let mut x = retain.try_owning().unwrap();
        *x = 58;
        // `Owning` 现在实现了 Drop，因此借用持续到它被析构；要归还 retain 必须先放手
        drop(x);
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
