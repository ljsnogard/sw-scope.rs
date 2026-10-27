mod pool_;
mod slot_;

#[cfg(test)]
mod tests_;

pub(crate) use pool_::WeakPool;
pub(crate) use slot_::{
    DataState, WeakSlot, data_ptr_of_, meta_from_raw_
};

#[cfg(test)]
pub(crate) use slot_::meta_to_raw_;
