use anyhow::Result;

use crate::layout::LayoutRoot;
use crate::ref_store::CasResult;

pub type LayoutRootVersion = u64;

pub trait LayoutRootStore {
    fn read_layout_root(&self) -> Result<LayoutRoot>;
    fn compare_and_swap_layout_root(
        &self,
        expected: LayoutRootVersion,
        next: LayoutRoot,
    ) -> Result<CasResult>;
    fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>>;
}
