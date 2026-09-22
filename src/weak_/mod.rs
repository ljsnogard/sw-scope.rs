mod chunk_;
mod pool_;
// 基础设施先行：Batch B（弱槽位换用 `PreDropRecord`）与 Batch C（公开 API + 注册表接入）
// 落地之前，本模块在非测试构建里暂时没有使用者，因此整体放行 dead_code 告警。
#[allow(dead_code)]
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

// 以下三项同为分批落地的基础设施，暂未被生产路径消费（见上面的说明）
#[allow(unused_imports)]
pub(crate) use pre_drop_::PreDropRecord;

// 类型级注册表依赖 `alloc::vec::Vec`，因此只在 `core-alloc` 下提供
#[allow(unused_imports)]
#[cfg(feature = "core-alloc")]
pub(crate) use pre_drop_::resolve_;
#[allow(unused_imports)]
#[cfg(feature = "core-alloc")]
pub(crate) use pre_drop_::PreDropRegistry;

// 以下几个类型目前只被测试直接引用，独立成行以便日后按需公开
#[cfg(test)]
pub(crate) use chunk_::WeakChunkState;
