use std::boxed::Box;

#[cfg(test)]
use crate::{test_support_::test_root_, *};

#[cfg(test)]
#[compio::test]
async fn demo() {
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

    let scope = Scope::new_in(test_root_()).expect("建域应当成功");
    let retain = scope.put_async(42).await.unwrap();
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
