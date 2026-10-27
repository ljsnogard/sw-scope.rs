pub trait TrPageSizeSelect {
    const PAGE_SIZE: usize;
}

pub struct PageSize1KB;
pub struct PageSize2KB;
pub struct PageSize4KB;
pub struct PageSize8KB;
pub struct PageSize16KB;
pub struct PageSize32KB;
pub struct PageSize64KB;

impl TrPageSizeSelect for PageSize1KB {
    const PAGE_SIZE: usize = 1024;
}
impl TrPageSizeSelect for PageSize2KB {
    const PAGE_SIZE: usize = 2 * 1024;
}
impl TrPageSizeSelect for PageSize4KB {
    const PAGE_SIZE: usize = 4 * 1024;
}
impl TrPageSizeSelect for PageSize8KB {
    const PAGE_SIZE: usize = 8 * 1024;
}
impl TrPageSizeSelect for PageSize16KB {
    const PAGE_SIZE: usize = 16 * 1024;
}
impl TrPageSizeSelect for PageSize32KB {
    const PAGE_SIZE: usize = 32 * 1024;
}
impl TrPageSizeSelect for PageSize64KB {
    const PAGE_SIZE: usize = 64 * 1024;
}
