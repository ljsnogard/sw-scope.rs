mod chunk_;
mod pool_;

#[cfg(test)]
mod tests_;

pub(crate) use chunk_::drop_entry_;
pub(crate) use chunk_::meta_from_raw_;
pub(crate) use chunk_::meta_of_;
pub(crate) use chunk_::meta_to_raw_;
pub(crate) use chunk_::WeakChunk;
pub(crate) use pool_::WeakPool;

// 以下几个类型目前只被测试直接引用，独立成行以便日后按需公开
#[cfg(test)]
pub(crate) use chunk_::DataState;
#[cfg(test)]
pub(crate) use chunk_::DropVtable;
#[cfg(test)]
pub(crate) use chunk_::WeakChunkState;
