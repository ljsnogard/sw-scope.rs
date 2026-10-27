mod chunk_;
mod pool_;
mod pre_drop_;

#[cfg(test)]
mod tests_;

#[cfg(test)]
mod pre_drop_tests_;

pub(crate) use chunk_::{MetaAddr, StrongChunk, StrongChunkBase};
pub(crate) use pool_::StrongPool;
#[cfg(test)]
pub(crate) use pre_drop_::PreDropRecord;
#[cfg(feature = "core-alloc")]
pub(crate) use pre_drop_::PreDropRegistry;
#[cfg(all(test, feature = "core-alloc"))]
pub(crate) use pre_drop_::resolve_;
