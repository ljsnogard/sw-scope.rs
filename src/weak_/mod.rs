mod pool_;
mod slot_;

#[cfg(test)]
mod tests_;

pub(crate) use pool_::WeakPool;
pub(crate) use slot_::{DataState, WeakSlot, meta_from_raw_, meta_of_, meta_to_raw_};
