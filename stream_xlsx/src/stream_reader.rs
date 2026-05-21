use crate::excel_types::{Cell, Data, Dimensions};
use std::path::Path;

pub trait StreamReader: Sized {
    fn new<P: AsRef<Path>>(
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
    ) -> anyhow::Result<Self>;
    fn dimensions(&self) -> Dimensions;
    fn next_cell(&mut self) -> anyhow::Result<Option<Cell<Data>>>;
}
