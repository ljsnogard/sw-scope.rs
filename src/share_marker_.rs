use crate::abs_::TrShareMarker;

/// 线程局部 Scope 模式 marker。
pub enum Local {}

/// 预留的跨线程 Scope 模式 marker。
pub enum Shared {}

impl TrShareMarker for Local {}
impl TrShareMarker for Shared {}

impl !Send for Local {}
impl !Sync for Local {}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}
