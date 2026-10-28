mod pool_;
mod slot_;

#[cfg(test)]
mod tests_;

pub(crate) use pool_::WeakPool;
// `meta_to_raw_` 供分配管线把 `?Sized` 的每对象元数据搬成裸字，直接写进强块 meta 槽；
// 类型表只保存类型级 vtable，不承载每对象长度。
#[allow(unused_imports)]
pub(crate) use slot_::meta_to_raw_;
pub(crate) use slot_::{DataState, WeakSlot, data_ptr_of_, meta_from_raw_};
