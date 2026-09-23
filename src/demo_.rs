use std::boxed::Box;

#[cfg(test)]
use crate::*;

#[cfg(test)]
#[test]
fn local_demo() {
    fn reown_somewhere(retain: Retain<usize>) -> Retain<usize> {
        let mut x = retain.try_owning().unwrap();
        *x = 58;
        // 我们故意不 drop 来展示 Retain 的作用
        // drop(x);
        retain.clone()
    }

    fn share_everywhere(retain: Retain<usize>) -> Retain<usize> {
        // 依然可以成功尽管强引用一度归零，但 retain 还在，scope 也在
        let x = retain.try_sharing().unwrap();
        assert_eq!(*x, 58);

        // 手动制造一个泄露，下次 try_owning 就会失败
        let _ = Box::leak(Box::new(x));
        retain
    }

    let mut scope = Scope::new_local();
    let x = Owning::try_new_local(42, &mut scope).unwrap();
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
    let retain = share_everywhere(retain);

    assert!(retain.try_owning().is_err());
    let shared = retain.try_sharing().unwrap();
    assert_eq!(*shared, 58);
}
