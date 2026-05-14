use crate::stream_xlsx::excel_types::{Cell, Data};
use calamine::{DataRef, Sheets, open_workbook_auto};
use polars::prelude::*;
use std::path::Path;
use std::str::FromStr;
use std::sync::mpsc;
use std::thread;

/// 把 calamine 的 Data 转换为我们独立的 Data
fn calamine_data_to_owned(d: &calamine::Data) -> Data {
    match d {
        calamine::Data::Int(v) => Data::Int(*v),
        calamine::Data::Float(v) => Data::Float(*v),
        calamine::Data::String(v) => Data::String(v.clone()),
        calamine::Data::Bool(v) => Data::Bool(*v),
        calamine::Data::DateTime(v) => {
            // calamine 未公开 is_1904，默认按 1900 epoch 处理
            Data::DateTime(crate::stream_xlsx::excel_types::ExcelDateTime::new(
                v.as_f64(),
                false,
            ))
        }
        calamine::Data::DateTimeIso(v) => Data::DateTimeIso(v.clone()),
        calamine::Data::DurationIso(v) => Data::DurationIso(v.clone()),
        calamine::Data::Error(e) => Data::Error(
            crate::stream_xlsx::excel_types::CellErrorType::from_str(&e.to_string())
                .unwrap_or(crate::stream_xlsx::excel_types::CellErrorType::Value),
        ),
        calamine::Data::Empty => Data::Empty,
    }
}

/// 流式读取所需要的列
#[derive(Debug)]
pub struct Col<T> {
    #[allow(dead_code)]
    y: u32,
    pub vec: Vec<T>,
}

impl<T: FromData> Col<T> {
    pub fn new(y: u32) -> Self {
        Self { y, vec: Vec::new() }
    }

    pub fn push_cell(&mut self, cell: Cell<Data>) -> anyhow::Result<()> {
        let (cell_x, cell_y) = cell.get_position();
        if cell_y != self.y {
            return Err(anyhow::anyhow!(
                "列序号错误: 期望 {}, 实际 {}",
                self.y,
                cell_y
            ));
        }
        let empty_num = (cell_x as usize).saturating_sub(self.vec.len());
        if empty_num > 0 {
            self.vec
                .extend(std::iter::repeat_with(|| T::from_data(&Data::Empty)).take(empty_num));
        }
        self.vec.push(T::from_data(cell.get_value()));
        Ok(())
    }
}

pub trait FromData: Sized {
    fn from_data(data: &Data) -> Self;
}

impl FromData for Data {
    fn from_data(data: &Data) -> Self {
        data.clone()
    }
}

impl FromData for AnyValue<'static> {
    fn from_data(data: &Data) -> Self {
        match data {
            Data::Int(v) => AnyValue::Int64(*v),
            Data::Float(v) => AnyValue::Float64(*v),
            Data::Bool(v) => AnyValue::Boolean(*v),
            Data::String(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
            Data::DateTime(v) => {
                let (y, m, d, h, min, s, ms) = v.to_ymd_hms_milli();
                let naive = chrono::NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
                    .and_then(|d| d.and_hms_milli_opt(h as u32, min as u32, s as u32, ms as u32))
                    .unwrap_or_default();
                AnyValue::Datetime(
                    naive.and_utc().timestamp_nanos_opt().unwrap_or(0),
                    TimeUnit::Nanoseconds,
                    None,
                )
            }
            Data::DateTimeIso(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
            Data::DurationIso(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
            Data::Error(_) => AnyValue::Null,
            Data::Empty => AnyValue::Null,
        }
    }
}

/// 兼容旧版 calamine DataRef 的转换（保留供 calamine 原生接口使用）
pub trait FromDataRef<'a>: Sized {
    fn from_data_ref(data: &DataRef<'a>) -> Self;
}

impl<'a> FromDataRef<'a> for DataRef<'a> {
    fn from_data_ref(data: &DataRef<'a>) -> Self {
        data.clone()
    }
}

impl<'a> FromDataRef<'a> for AnyValue<'a> {
    fn from_data_ref(data: &DataRef<'a>) -> Self {
        match data {
            DataRef::Int(v) => AnyValue::Int64(*v),
            DataRef::Float(v) => AnyValue::Float64(*v),
            DataRef::Bool(v) => AnyValue::Boolean(*v),
            DataRef::String(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
            DataRef::SharedString(v) => AnyValue::String(v),
            DataRef::DateTime(v) => {
                let (y, m, d, h, min, s, ms) = v.to_ymd_hms_milli();
                let naive = chrono::NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
                    .and_then(|d| d.and_hms_milli_opt(h as u32, min as u32, s as u32, ms as u32))
                    .unwrap_or_default();
                AnyValue::Datetime(
                    naive.and_utc().timestamp_nanos_opt().unwrap_or(0),
                    TimeUnit::Nanoseconds,
                    None,
                )
            }
            DataRef::DateTimeIso(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
            DataRef::DurationIso(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
            DataRef::Error(_) => AnyValue::Null,
            DataRef::Empty => AnyValue::Null,
        }
    }
}

/// 流式读取所需要的多个列名为 Cols
#[derive(Debug)]
pub struct Cols<T> {
    pub vecs: Vec<Col<T>>,
    pub max_x: usize,
    pub cell_cache: Option<Cell<Data>>,
    pub col_num: usize,
    pub headers: Vec<String>,
    pub first_batch_done: bool,
}

impl<T> Cols<T>
where
    T: FromData,
{
    pub fn new(num: usize) -> Self {
        let mut vecs: Vec<Col<T>> = Vec::with_capacity(num + 1);
        for i in 0..=num {
            vecs.push(Col::new(i as u32));
        }
        Self {
            vecs,
            max_x: 0,
            cell_cache: None,
            col_num: num,
            headers: Vec::with_capacity(num),
            first_batch_done: false,
        }
    }

    pub fn push_cell(&mut self, cell: Cell<Data>) -> anyhow::Result<()> {
        let (x, y) = cell.get_position();
        let x = x as usize;
        let y = y as usize;
        if x > self.max_x {
            self.max_x = x;
        }
        if y >= self.vecs.len() {
            let start = self.vecs.len();
            for i in start..=y {
                self.vecs.push(Col::new(i as u32));
            }
        }
        let col = self
            .vecs
            .get_mut(y)
            .ok_or_else(|| anyhow::anyhow!("列 {} 超出预定义范围", y))?;
        col.push_cell(cell)
    }

    pub fn clean_for_iter(&self) {}
}

impl Cols<AnyValue<'static>> {
    pub fn into_dataframe(&mut self, has_header: bool) -> PolarsResult<DataFrame> {
        let max_len = self.vecs.iter().map(|c| c.vec.len()).max().unwrap_or(0);
        let is_first_batch = !self.first_batch_done;
        if is_first_batch {
            for i in 0..self.col_num {
                let header = if has_header && !self.vecs[i].vec.is_empty() {
                    match &self.vecs[i].vec[0] {
                        AnyValue::String(s) => s.to_string(),
                        AnyValue::StringOwned(s) => s.to_string(),
                        other => format!("{}", other),
                    }
                } else {
                    format!("col_{}", i)
                };
                self.headers.push(header);
            }
            self.first_batch_done = true;
        }
        let old_vecs = std::mem::replace(
            &mut self.vecs,
            (0..self.col_num).map(|i| Col::new(i as u32)).collect(),
        );
        let columns: Vec<Column> = old_vecs
            .into_iter()
            .enumerate()
            .map(|(i, mut col)| {
                if col.vec.len() < max_len {
                    col.vec.resize(max_len, AnyValue::Null);
                }
                let name = self.headers.get(i).map(|s| s.as_str()).unwrap_or("unknown");
                let values = if is_first_batch && has_header {
                    &col.vec[1..]
                } else {
                    &col.vec[..]
                };

                Series::new(name.into(), values).into()
            })
            .collect();

        DataFrame::new_infer_height(columns)
    }

    /// 流式读取 xlsx，按行分批返回 DataFrame（使用后台线程 + calamine）
    pub fn df_iter<P>(
        batch_size: usize,
        path: P,
        sheet_name: &str,
    ) -> anyhow::Result<impl Iterator<Item = DataFrame>>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref().to_owned();
        let sheet_name = sheet_name.to_owned();
        let (tx, rx) = mpsc::channel::<DataFrame>();

        thread::spawn(move || {
            let send_all = || -> anyhow::Result<()> {
                let wb = open_workbook_auto(&path)?;
                let mut xlsx = match wb {
                    Sheets::Xlsx(sheet) => sheet,
                    _ => return Err(anyhow::anyhow!("非xlsx")),
                };
                let mut cr = xlsx.worksheet_cells_reader(&sheet_name)?;
                let dim = cr.dimensions();
                let bs = batch_size.min(dim.end.1 as usize);

                let mut cols = Cols::<AnyValue>::new(bs);
                let mut cell_cache: Option<calamine::Cell<DataRef>> = None;

                loop {
                    if let Some(cell) = cell_cache.take() {
                        let owned =
                            calamine_data_to_owned(&calamine::Data::from(cell.get_value().clone()));
                        cols.push_cell(Cell::new(cell.get_position(), owned))?;
                    }

                    let mut batch_boundary = false;
                    while let Some(cell) = cr.next_cell()? {
                        let cell_x = cell.get_position().0;
                        if cell_x as usize > cols.max_x {
                            cell_cache = Some(cell);
                            batch_boundary = true;
                            break;
                        }
                        let owned =
                            calamine_data_to_owned(&calamine::Data::from(cell.get_value().clone()));
                        cols.push_cell(Cell::new(cell.get_position(), owned))?;
                        cols.clean_for_iter();
                    }

                    if batch_boundary {
                        let df = cols.into_dataframe(true)?;
                        if tx.send(df).is_err() {
                            return Ok(());
                        }
                        continue;
                    }

                    let has_data = cols.vecs.iter().any(|c| !c.vec.is_empty());
                    if has_data {
                        let df = cols.into_dataframe(true)?;
                        let _ = tx.send(df);
                    }
                    break;
                }

                Ok(())
            };

            if let Err(e) = send_all() {
                eprintln!("df_iter error: {e}");
            }
        });

        Ok(rx.into_iter())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_cols() -> anyhow::Result<()> {
        use super::Cols;
        use calamine::{Sheets, open_workbook_auto};
        use polars::prelude::AnyValue;

        let wb = open_workbook_auto("test_data.xlsx")?;
        let mut xlsx = match wb {
            Sheets::Xlsx(sheet) => sheet,
            _ => return Err(anyhow::anyhow!("非xlsx")),
        };

        let mut cr = xlsx.worksheet_cells_reader("Sheet1")?;
        let dim = cr.dimensions();
        let mut cols = Cols::<AnyValue>::new(dim.end.1 as usize);

        while let Some(cell) = cr.next_cell()? {
            let owned =
                super::calamine_data_to_owned(&calamine::Data::from(cell.get_value().clone()));
            cols.push_cell(super::Cell::new(cell.get_position(), owned))?;
        }

        let df = cols.into_dataframe(true)?;
        println!("{}", df);

        Ok(())
    }
}

#[cfg(test)]
mod iter_tests {
    use super::Cols;

    #[test]
    fn test_df_iter() -> anyhow::Result<()> {
        let iter = Cols::df_iter(10, "test_data.xlsx", "Sheet1")?;
        let mut total_rows = 0;
        for (i, df) in iter.enumerate() {
            println!("batch {}: shape {:?}", i, df.shape());
            total_rows += df.height();
        }
        println!("total rows: {}", total_rows);
        Ok(())
    }
}
