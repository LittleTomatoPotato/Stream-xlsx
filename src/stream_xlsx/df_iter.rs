use crate::stream_xlsx::{
    excel_types::{Cell, Data, Dimensions},
    xlsx_stream_unsafe::XlsxStreamReader,
};
use polars::prelude::DataFrame;
use polars::{
    datatypes::{AnyValue, PlSmallStr, TimeUnit},
    error::PolarsResult,
    frame::column::Column,
    prelude::NamedFrom,
    series::Series,
};
use std::{path::Path, usize};

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

    pub fn push_cell(&mut self, cell: Cell<Data>, batch_row: usize) {
        let empty_num = (batch_row).saturating_sub(self.vec.len());
        if empty_num > 0 {
            self.vec
                .extend(std::iter::repeat_with(|| T::from_data(&Data::Empty)).take(empty_num));
        }
        self.vec.push(T::from_data(cell.get_value()));
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
    pub batch_size: usize,
    pub cell_cache: Option<Cell<Data>>,
    pub col_num: usize,
    pub headers: Vec<String>,
}

impl<T> Cols<T>
where
    T: FromData,
{
    pub fn new(dimension: &Dimensions, batch_size: usize) -> Self {
        let col_num = dimension.end.1 as usize + 1;
        let mut vecs: Vec<Col<T>> = Vec::with_capacity(col_num);
        for i in 0..=col_num {
            vecs.push(Col::new(i));
        }
        Self {
            vecs,
            batch_size,
            cell_cache: None,
            col_num: col_num,
            headers: Vec::with_capacity(col_num),
        }
    }

    pub fn push_cell(&mut self, cell: Cell<Data>, batch_row: usize) -> anyhow::Result<()> {
        let (_, y) = cell.get_position();
        let y = y as usize;
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
        col.push_cell(cell, batch_row);
        Ok(())
    }
}

impl Cols<AnyValue<'static>> {
    pub fn into_dataframe(&mut self) -> PolarsResult<DataFrame> {
        let max_len = self.vecs.iter().map(|c| c.vec.len()).max().unwrap_or(0);
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
                let values = &col.vec[..];

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
    has_header: bool,
    len: usize,                      // 总批次数
    batch_start_row: Option<u32>,    // 当前批次的起始绝对行号
    current_row_count: usize,        // 当前批次已收集的行数（用于批次截断）
    last_processed_row: Option<u32>, // 上一个处理的绝对行号（检测行切换)
}

impl DataFrameIter {
    pub fn new<P>(
        batch_size: usize,
        path: P,
        sheet_name: &str,
        has_header: bool,
    ) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        let reader = XlsxStreamReader::new(path, sheet_name)?;
        let dim = reader.dimensions();
        let cols = Cols::new(&dim, batch_size);
        let mut iter = Self {
            reader,
            cols,
            cell_cache: None,
            has_header,
            len: 0,
            batch_start_row: None,
            current_row_count: 0,
            last_processed_row: None,
        };
        iter.find_header(batch_size);

        Ok(iter)
    }

    fn find_header(&mut self, batch_size: usize) {
        // 应该寻找第一个非空行
        let first_cell = match self.reader.next_cell().ok().flatten() {
            Some(cell) => cell,
            None => {
                self.cols
                    .vecs
                    .iter()
                    .enumerate()
                    .for_each(|(i, _)| self.cols.headers.push(format!("col_{}", i)));
                self.len = 0;
                return;
            }
        };
        let first_x = first_cell.get_position().0;
        let total_rows: usize; // 计算迭代器长度
        if self.has_header {
            // 将非空的第一行放入header中
            self.cols.headers.push(first_cell.into());
            while let Some(cell) = self.reader.next_cell().ok().flatten() {
                let x = cell.get_position().0;
                if x == first_x {
                    self.cols.headers.push(cell.into());
                } else {
                    self.cell_cache = Some(cell);
                    break;
                }
            }

            total_rows = (self.reader.dimensions().end.0 - first_x) as usize;
        } else {
            self.cell_cache = Some(first_cell);
            self.cols
                .vecs
                .iter()
                .enumerate()
                .for_each(|(i, _)| self.cols.headers.push(format!("col_{}", i)));
            total_rows = (self.reader.dimensions().end.0 - first_x + 1) as usize;
        }
        self.len = (total_rows + batch_size - 1) / batch_size;
    }

    fn finsh_batch(&mut self) -> Option<DataFrame> {
        let has_data = self.cols.vecs.iter().any(|c| !c.vec.is_empty());
        if !has_data {
            return None;
        }
        let df = self.cols.into_dataframe().ok();
        // 重置状态，准备下一批
        self.batch_start_row = None;
        self.current_row_count = 0;
        self.last_processed_row = None;
        df
    }
}

impl Iterator for DataFrameIter {
    type Item = DataFrame;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(cell) = self.cell_cache.take() {
            let cell_x = cell.get_position().0;
            self.batch_start_row = Some(cell_x);
            self.current_row_count = 1;
            self.last_processed_row = Some(cell_x);
            self.cols.push_cell(cell, 0).ok()?;
        } else if self.batch_start_row.is_none() {
            match self.reader.next_cell().ok().flatten() {
                Some(cell) => {
                    let cell_x = cell.get_position().0;
                    self.batch_start_row = Some(cell_x);
                    self.current_row_count = 1;
                    self.last_processed_row = Some(cell_x);
                    self.cols.push_cell(cell, 0).ok()?;
                }
                None => {
                    return None;
                }
            }
        }

        loop {
            match self.reader.next_cell() {
                Ok(Some(cell)) => {
                    let current_row = cell.get_position().0;

                    // 检测行切换
                    if self.last_processed_row.map_or(true, |lr| lr != current_row) {
                        // 新的一行：先检查是否已经达到批次大小
                        if self.current_row_count >= self.cols.batch_size {
                            // 缓存当前单元格，返回当前批次
                            self.cell_cache = Some(cell);
                            return self.finsh_batch();
                        }
                        // 未达批次，进入新行
                        self.current_row_count += 1;
                        self.last_processed_row = Some(current_row);
                    }

                    let batch_row = (current_row - self.batch_start_row.unwrap()) as usize;
                    // 正常推入单元格
                    self.cols.push_cell(cell, batch_row).ok()?;
                }
                Ok(None) => {
                    let has_data = self.cols.vecs.iter().any(|c| !c.vec.is_empty());
                    if has_data {
                        return self.cols.into_dataframe().ok();
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
pub fn df_iter<P>(
    batch_size: usize,
    path: P,
    sheet_name: &str,
    has_header: bool,
) -> anyhow::Result<DataFrameIter>
where
    P: AsRef<Path>,
{
    DataFrameIter::new(batch_size, path, sheet_name, has_header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_df_iter() -> anyhow::Result<()> {
        let iter = df_iter(10, "test_data.xlsx", "Sheet1", true)?;
        let mut total_rows = 0;
        for (i, df) in iter.enumerate() {
            if i <= 5 {
                println!("batch {}: shape {:?}", i, df.shape());
                println!("{}", df)
            }
            total_rows += df.height();
        }
        println!("total rows: {}", total_rows);
        Ok(())
    }
}
