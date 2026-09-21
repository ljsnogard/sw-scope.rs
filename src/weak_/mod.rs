mod chunk_;

#[cfg(test)]
mod tests_;

pub(crate) use chunk_::{
    DataState, WeakChunk, WeakChunkState, WeakChunkStateGuard,
};
