use core::mem::MaybeUninit;

use crate::{
    index_::Retain,
    scope_str_::ScopeStr,
};

pub trait TrScope {
    type Err;

    fn try_put<T>(self, data: T) -> Result<Retain<T>, Self::Err>;

    fn try_put_str(self, str: &str) -> Result<Retain<ScopeStr>, Self::Err>;

    fn try_clone<T>(self, src: &[T]) -> Result<Retain<[T]>, Self::Err>
    where
        T: Clone;

    fn try_emplace<F, T>(self, emplace: F) -> Result<Retain<T>, Self::Err>
    where
        F: FnOnce(&mut MaybeUninit<T>) -> &mut T,
        T: Sized;

    fn try_alloc_slice_uninit<T>(
        self,
        length: usize,
    ) -> Result<Retain<[MaybeUninit<T>]>, Self::Err>;

    fn put<T>(self, data: T) -> Retain<T> where Self: Sized {
        let Result::Ok(w) = self.try_put(data) else {
            panic!()
        };
        w
    }

    fn put_str(self, str: &str) -> Retain<ScopeStr> where Self: Sized {
        let Result::Ok(w) = self.try_put_str(str) else {
            panic!()
        };
        w
    }
}
