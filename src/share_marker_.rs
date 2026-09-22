use crate::abs_::TrShareMarker;

/// 线程局部 Scope 模式 marker。
///
/// 当前只实现该模式：Root / Scope / 句柄都不跨线程。`Shared` marker 先预留为默认参数
/// 的另一侧，后续实现跨线程 Scope 时再补齐约束与锁。
pub enum Local {}

/// 预留的跨线程 Scope 模式 marker。
pub enum Shared {}

impl TrShareMarker for Local {}
impl TrShareMarker for Shared {}
