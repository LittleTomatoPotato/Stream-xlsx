use crate::excel_types::{Cell, Data, Dimensions};

pub trait StreamReader: Sized {
    fn dimensions(&self) -> Dimensions;
    fn next_cell(&mut self) -> anyhow::Result<Option<Cell<Data>>>;
}
