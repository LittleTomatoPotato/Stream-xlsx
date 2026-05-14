use crate::stream_xlsx::{
    excel_types::{Cell, Data, Dimensions},
    xlsx_stream::XlsxStreamReader,
};
use polars::prelude::DataFrame;
use polars::{
    datatypes::{AnyValue, PlSmallStr, TimeUnit},
    error::PolarsResult,
    frame::column::Column,
    prelude::NamedFrom,
    series::Series,
};
use std::path::Path;

// 流式读取所需要的列
#[derive(Debug)]
pub struct Col<T> {
    #[allow(dead_code)]
    y: usize,
    pub vec: Vec<T>,
}

impl<T: FromData> Col<T> {
    pub fn new(y: usize) -> Self {
        Self { y, vec: Vec::new() }
    }

    pub fn push_cell(&mut self, cell: Cell<Data>) -> anyhow::Result<()> {
        let (cell_x, cell_y) = cell.get_position();
        if cell_y as usize != self.y {
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
use calamine::DataRef;
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
    pub fn new(dimension: &Dimensions, batch_size: usize) -> Self {
        let col_num = dimension.end.1 as usize;
        let bs = batch_size.min(dimension.end.0 as usize);
        let mut vecs: Vec<Col<T>> = Vec::with_capacity(bs);
        for i in 0..=col_num {
            vecs.push(Col::new(i));
        }
        Self {
            vecs,
            max_x: 0,
            cell_cache: None,
            col_num: col_num,
            headers: Vec::with_capacity(col_num),
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
                self.vecs.push(Col::new(i));
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
            (0..self.col_num).map(|i| Col::new(i)).collect(),
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
}
/// 单线程流式 xlsx DataFrame 迭代器。
///
/// 底层使用独立的 `XlsxStreamReader` 直接解压并解析 sheet XML，
/// 不依赖 calamine 的任何内部类型。
pub struct DataFrameIter {
    reader: XlsxStreamReader,
    cols: Cols<polars::prelude::AnyValue<'static>>,
    cell_cache: Option<Cell<Data>>,
}

impl DataFrameIter {
    pub fn new<P>(batch_size: usize, path: P, sheet_name: &str) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        let reader = XlsxStreamReader::new(path, sheet_name)?;
        let dim = reader.dimensions();
        let cols = Cols::new(&dim, batch_size);
        Ok(Self {
            reader,
            cols,
            cell_cache: None,
        })
    }
}

impl Iterator for DataFrameIter {
    type Item = DataFrame;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(cell) = self.cell_cache.take() {
            if let Err(e) = self.cols.push_cell(cell) {
                eprintln!("push cached cell failed: {e}");
                return None;
            }
        }

        loop {
            match self.reader.next_cell() {
                Ok(Some(cell)) => {
                    let cell_x = cell.get_position().0;
                    if cell_x as usize > self.cols.max_x {
                        self.cell_cache = Some(cell);
                        return self.cols.into_dataframe(true).ok();
                    }
                    if let Err(e) = self.cols.push_cell(cell) {
                        eprintln!("push cell failed: {e}");
                        return None;
                    }
                    self.cols.clean_for_iter();
                }
                Ok(None) => {
                    let has_data = self.cols.vecs.iter().any(|c| !c.vec.is_empty());
                    if has_data {
                        return self.cols.into_dataframe(true).ok();
                    }
                    return None;
                }
                Err(e) => {
                    eprintln!("DataFrameIter read error: {e}");
                    return None;
                }
            }
        }
    }
}

/// 便捷函数：直接返回一个 DataFrame 迭代器
pub fn df_iter<P>(batch_size: usize, path: P, sheet_name: &str) -> anyhow::Result<DataFrameIter>
where
    P: AsRef<Path>,
{
    DataFrameIter::new(batch_size, path, sheet_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_df_iter() -> anyhow::Result<()> {
        let iter = df_iter(10, "test_data.xlsx", "Sheet1")?;
        let mut total_rows = 0;
        for (i, df) in iter.enumerate() {
            println!("batch {}: shape {:?}", i, df.shape());
            total_rows += df.height();
        }
        println!("total rows: {}", total_rows);
        Ok(())
    }
}
