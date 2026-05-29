use crate::{
    excel_types::{Cell, Data, Dimensions},
    workbook::XlsxWorkbook,
    xlsx_stream_lm::XlsxStreamReader,
};
use polars::prelude::DataFrame;
use polars::{
    datatypes::{AnyValue, DataType, PlSmallStr, TimeUnit},
    error::PolarsResult,
    frame::column::Column,
    prelude::NamedFrom,
    series::Series,
};
use std::{path::Path, sync::Arc, usize};

// 流式读取所需要的列
#[derive(Debug)]
pub struct Col<T> {
    #[allow(dead_code)]
    y: usize,
    pub vec: Vec<T>,
}

impl<T: FromData> Col<T> {
    pub fn new(y: usize, capacity: usize) -> Self {
        Self {
            y,
            vec: Vec::with_capacity(capacity),
        }
    }

    pub fn push_cell(&mut self, data: Data, batch_row: usize) {
        let empty_num = (batch_row).saturating_sub(self.vec.len());
        if empty_num > 0 {
            self.vec
                .extend(std::iter::repeat_with(|| T::from_data(Data::Empty)).take(empty_num));
        }
        self.vec.push(T::from_data(data));
    }
}

pub trait FromData: Sized {
    fn from_data(data: Data) -> Self;
}

impl FromData for Data {
    fn from_data(data: Data) -> Self {
        data
    }
}

impl FromData for AnyValue<'static> {
    fn from_data(data: Data) -> Self {
        match data {
            Data::Int(v) => AnyValue::Int64(v),
            Data::Float(v) => AnyValue::Float64(v),
            Data::Bool(v) => AnyValue::Boolean(v),
            Data::String(v) => AnyValue::StringOwned(PlSmallStr::from(v)),
            Data::DateTime(v) => {
                AnyValue::Datetime(v.to_timestamp_nanos(), TimeUnit::Nanoseconds, None)
            }
            Data::DateTimeIso(v) => AnyValue::StringOwned(PlSmallStr::from(v)),
            Data::DurationIso(v) => AnyValue::StringOwned(PlSmallStr::from(v)),
            Data::Error(_) => AnyValue::Null,
            Data::Empty => AnyValue::Null,
        }
    }
}

/// 流式读取所需要的多个列名为 Cols
#[derive(Debug)]
pub struct Cols<T> {
    pub vecs: Vec<Col<T>>,
    pub batch_size: usize,
    pub col_num: usize,
    pub headers: Vec<String>,
    pub col_dtypes: Vec<Option<DataType>>,
}

impl<T> Cols<T>
where
    T: FromData,
{
    pub fn new(dimension: &Dimensions, batch_size: usize) -> Self {
        let col_num = dimension.end.1 as usize + 1;
        let mut vecs: Vec<Col<T>> = Vec::with_capacity(col_num);
        for i in 0..col_num {
            vecs.push(Col::new(i, batch_size));
        }
        Self {
            vecs,
            batch_size,
            col_num,
            headers: Vec::with_capacity(col_num),
            col_dtypes: vec![None; col_num],
        }
    }

    pub fn push_cell(&mut self, cell: Cell<Data>, batch_row: usize) -> anyhow::Result<()> {
        let (_, y) = cell.get_position();
        let y = y as usize;
        if y >= self.vecs.len() {
            let start = self.vecs.len();
            for i in start..=y {
                self.vecs.push(Col::new(i, self.batch_size));
                self.col_dtypes.push(None);
            }
        }
        // 边写边推断类型（必须在 get_mut 之前，避免重复可变借用）
        self.infer_dtype(y, cell.get_value());
        let col = self
            .vecs
            .get_mut(y)
            .ok_or_else(|| anyhow::anyhow!("列 {} 超出预定义范围", y))?;
        col.push_cell(cell.into_value(), batch_row);
        Ok(())
    }

    fn infer_dtype(&mut self, col_idx: usize, data: &Data) {
        if matches!(data, Data::Empty | Data::Error(_)) {
            return;
        }
        let new_dtype = match data {
            Data::Int(_) => DataType::Int64,
            Data::Float(_) => DataType::Float64,
            Data::Bool(_) => DataType::Boolean,
            Data::String(_) | Data::DateTimeIso(_) | Data::DurationIso(_) => DataType::String,
            Data::DateTime(_) => DataType::Datetime(TimeUnit::Nanoseconds, None),
            _ => DataType::Null,
        };
        let current = &mut self.col_dtypes[col_idx];
        *current = match (current.take(), new_dtype) {
            (None, dt) => Some(dt),
            (Some(DataType::Int64), DataType::Float64)
            | (Some(DataType::Float64), DataType::Int64) => Some(DataType::Float64),
            (Some(dt1), dt2) if dt1 == dt2 => Some(dt1),
            _ => Some(DataType::String),
        };
    }
}

impl Cols<AnyValue<'static>> {
    pub fn into_dataframe(&mut self) -> PolarsResult<DataFrame> {
        let max_len = self.vecs.iter().map(|c| c.vec.len()).max().unwrap_or(0);
        let num_cols = self.vecs.len();
        let old_vecs = std::mem::replace(
            &mut self.vecs,
            (0..num_cols)
                .map(|i| Col::new(i, self.batch_size))
                .collect(),
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

                let series = if let Some(dt) = self.col_dtypes.get(i).and_then(|d| d.as_ref()) {
                    Series::from_any_values_and_dtype(name.into(), values, dt, false)
                } else {
                    Ok(Series::new(name.into(), values))
                }?;
                Ok::<_, polars::error::PolarsError>(series.into())
            })
            .collect::<Result<Vec<_>, _>>()?;

        DataFrame::new_infer_height(columns)
    }
}
/// 单线程流式 xlsx DataFrame 迭代器。
///
/// 底层使用独立的 `XlsxStreamReader` 直接解压并解析 sheet XML，
/// 不依赖 calamine 的任何内部类型。
pub struct DataFrameIter {
    workbook: Arc<XlsxWorkbook>,
    reader: XlsxStreamReader,
    cols: Cols<polars::prelude::AnyValue<'static>>,
    cell_cache: Option<Cell<Data>>,
    has_header: bool,
    len: usize,                      // 总批次数
    batch_start_row: Option<u32>,    // 当前批次的起始绝对行号
    current_row_count: usize,        // 当前批次已收集的行数（用于批次截断）
    last_processed_row: Option<u32>, // 上一个处理的绝对行号（检测行切换)
    current_sheet_name: Option<String>,
    current_sheet_idx: Option<usize>,
}

impl DataFrameIter {
    pub fn new<P>(
        batch_size: Option<usize>,
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
    ) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        let workbook = Arc::new(XlsxWorkbook::open(path)?);
        Self::from_workbook(batch_size, workbook, sheet_name, sheet_idx, has_header)
    }

    pub fn from_workbook(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
    ) -> anyhow::Result<Self> {
        let reader = XlsxStreamReader::from_workbook(Arc::clone(&workbook), sheet_name, sheet_idx)?;
        let dim = reader.dimensions();
        let batch_size = match batch_size {
            Some(s) => s,
            None => dim.end.0 as usize + if has_header { 0 } else { 1 },
        };
        let cols = Cols::new(&dim, batch_size);
        let mut iter = Self {
            workbook,
            reader,
            cols,
            cell_cache: None,
            has_header,
            len: 0,
            batch_start_row: None,
            current_row_count: 0,
            last_processed_row: None,
            current_sheet_name: sheet_name.map(|s| s.to_string()),
            current_sheet_idx: sheet_idx,
        };
        iter.find_header(batch_size)?;

        Ok(iter)
    }

    pub fn workbook(&self) -> &Arc<XlsxWorkbook> {
        &self.workbook
    }

    /// 切换到指定 sheet，重置所有解析状态。
    pub fn select_sheet(
        &mut self,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
    ) -> anyhow::Result<()> {
        self.reader =
            XlsxStreamReader::from_workbook(Arc::clone(&self.workbook), sheet_name, sheet_idx)?;
        let dim = self.reader.dimensions();
        let batch_size = self.cols.batch_size;
        self.cols = Cols::new(&dim, batch_size);
        self.cell_cache = None;
        self.batch_start_row = None;
        self.current_row_count = 0;
        self.last_processed_row = None;
        self.current_sheet_name = sheet_name.map(|s| s.to_string());
        self.current_sheet_idx = sheet_idx;
        self.len = 0;
        self.find_header(batch_size)?;
        Ok(())
    }

    fn find_header(&mut self, batch_size: usize) -> anyhow::Result<()> {
        let first_cell = match self.reader.next_cell()? {
            Some(cell) => cell,
            None => {
                self.cols
                    .vecs
                    .iter()
                    .enumerate()
                    .for_each(|(i, _)| self.cols.headers.push(format!("col_{}", i)));
                self.len = 0;
                return Ok(());
            }
        };
        let first_x = first_cell.get_position().0;
        let total_rows: usize;
        if self.has_header {
            self.cols.headers.push(first_cell.into());
            loop {
                match self.reader.next_cell()? {
                    Some(cell) => {
                        let (x, y) = cell.get_position();
                        if x == first_x {
                            while y > self.cols.headers.len() as u32 {
                                self.cols
                                    .headers
                                    .push(format!("Unknown_{}", self.cols.headers.len()));
                            }
                            let mut value: String = cell.into();
                            value = if value == "" {
                                format!("Unknown_{}", y)
                            } else {
                                value
                            };
                            self.cols.headers.push(value.into());
                        } else {
                            self.cell_cache = Some(cell);
                            let header_num = self.cols.headers.len() as u32;
                            let y = self.reader.dimensions().end.1;
                            if header_num <= y {
                                for i in header_num..=y {
                                    self.cols.headers.push(format!("Unknown_{}", i));
                                }
                            }
                            break;
                        }
                    }
                    None => break,
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
        Ok(())
    }

    fn finish_batch(&mut self) -> Option<anyhow::Result<DataFrame>> {
        let has_data = self.cols.vecs.iter().any(|c| !c.vec.is_empty());
        if !has_data {
            return None;
        }
        let df = match self.cols.into_dataframe() {
            Ok(df) => df,
            Err(e) => return Some(Err(anyhow::anyhow!("{e}"))),
        };
        self.batch_start_row = None;
        self.current_row_count = 0;
        self.last_processed_row = None;
        Some(Ok(df))
    }
}

impl Iterator for DataFrameIter {
    type Item = anyhow::Result<DataFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        // 从缓存或 reader 获取当前批次的第一个 cell
        if let Some(cell) = self.cell_cache.take() {
            let cell_x = cell.get_position().0;
            self.batch_start_row = Some(cell_x);
            self.current_row_count = 1;
            self.last_processed_row = Some(cell_x);
            if let Err(e) = self.cols.push_cell(cell, 0) {
                return Some(Err(e));
            }
        } else if self.batch_start_row.is_none() {
            match self.reader.next_cell() {
                Ok(Some(cell)) => {
                    let cell_x = cell.get_position().0;
                    self.batch_start_row = Some(cell_x);
                    self.current_row_count = 1;
                    self.last_processed_row = Some(cell_x);
                    if let Err(e) = self.cols.push_cell(cell, 0) {
                        return Some(Err(e));
                    }
                }
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            }
        }

        loop {
            match self.reader.next_cell() {
                Ok(Some(cell)) => {
                    let current_row = cell.get_position().0;

                    if self.last_processed_row.map_or(true, |lr| lr != current_row) {
                        if self.current_row_count >= self.cols.batch_size {
                            self.cell_cache = Some(cell);
                            self.len = self.len.saturating_sub(1);
                            return self.finish_batch();
                        }
                        self.current_row_count += 1;
                        self.last_processed_row = Some(current_row);
                    }

                    let start_row = match self.batch_start_row {
                        Some(r) => r,
                        None => {
                            return Some(Err(anyhow::anyhow!("batch_start_row lost mid-batch")));
                        }
                    };
                    let batch_row = (current_row - start_row) as usize;
                    if let Err(e) = self.cols.push_cell(cell, batch_row) {
                        return Some(Err(e));
                    }
                }
                Ok(None) => {
                    let has_data = self.cols.vecs.iter().any(|c| !c.vec.is_empty());
                    if has_data {
                        self.len = self.len.saturating_sub(1);
                        return self.finish_batch();
                    }
                    return None;
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl ExactSizeIterator for DataFrameIter {}

/// 便捷函数：直接返回一个 DataFrame 迭代器
pub fn df_iter(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
) -> anyhow::Result<DataFrameIter> {
    DataFrameIter::new(batch_size, path, sheet_name, sheet_idx, has_header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_df_iter() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let iter = df_iter(10.into(), &path, "Sheet1".into(), None, true)?;
        let mut total_rows = 0;
        for (i, batch) in iter.enumerate() {
            let df = batch?;
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

#[cfg(test)]
mod multi_sheet_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_workbook_two_sheets() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let wb = XlsxWorkbook::open(&path)?;
        let names = wb.sheet_names();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0], "Sheet1");
        assert_eq!(names[1], "Sheet2");
        Ok(())
    }

    #[test]
    fn test_select_sheet() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let wb = Arc::new(XlsxWorkbook::open(&path)?);
        let mut iter = DataFrameIter::from_workbook(Some(5), Arc::clone(&wb), Some("Sheet1"), None, true)?;

        let df1 = iter.next().unwrap()?;
        let rows1 = df1.height();
        println!("Sheet1 first batch: {} rows, cols: {:?}", rows1, df1.get_column_names());

        iter.select_sheet(Some("Sheet2"), None)?;
        let df2 = iter.next().unwrap()?;
        let rows2 = df2.height();
        println!("Sheet2 first batch: {} rows, cols: {:?}", rows2, df2.get_column_names());

        Ok(())
    }
}
