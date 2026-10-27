mod pool_;
mod slot_;

#[cfg(test)]
mod tests_;

pub(crate) use pool_::WeakPool;
// `meta_to_raw_` 供 Scope 层在把 `?Sized` 元数据登记进 `RootScope::metas_` 时使用；
// 分配管线本身只消费登记后的稳定地址。
#[allow(unused_imports)]
pub(crate) use slot_::meta_to_raw_;
pub(crate) use slot_::{DataState, WeakSlot, data_ptr_of_, meta_from_raw_};
