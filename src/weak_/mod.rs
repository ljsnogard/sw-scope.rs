mod chunk_;
mod pool_;
mod pre_drop_;

#[cfg(all(test, feature = "core-alloc"))]
mod pre_drop_tests_;

#[cfg(test)]
mod tests_;

pub(crate) use chunk_::meta_from_raw_;
pub(crate) use chunk_::meta_of_;
pub(crate) use chunk_::meta_to_raw_;
pub(crate) use chunk_::DataState;
pub(crate) use chunk_::WeakChunk;
pub(crate) use pool_::WeakPool;
pub(crate) use pre_drop_::PreDropRecord;

// 类型级注册表依赖 `alloc::vec::Vec`，因此只在 `core-alloc` 下提供
#[cfg(feature = "core-alloc")]
pub(crate) use pre_drop_::resolve_;
#[cfg(feature = "core-alloc")]
pub(crate) use pre_drop_::PreDropRegistry;

// 以下几个类型目前只被测试直接引用，独立成行以便日后按需公开
#[cfg(test)]
pub(crate) use chunk_::WeakChunkState;
