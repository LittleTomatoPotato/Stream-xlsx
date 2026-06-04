use crate::{
    excel_types::{Cell, Data, Dimensions},
    sheet_fast::SheetFastReader,
    workbook::{SharedStrings, XlsxWorkbook},
    xlsx_stream_lm::XlsxStreamReader,
};
use polars::prelude::*;
use polars_arrow::array::{Array, MutablePlString, Utf8ViewArray, View};
use polars_arrow::bitmap::{Bitmap, MutableBitmap};
use std::{path::Path, sync::Arc};

// ------------------------------------------------------------------
// TypedCol / TypedCols：按类型存储列数据，数值列实现零拷贝构建
// ------------------------------------------------------------------

#[derive(Debug)]
pub enum TypedCol {
    Int64(Vec<i64>, MutableBitmap),
    Float64(Vec<f64>, MutableBitmap),
    Bool(Vec<bool>, MutableBitmap),
    String(MutablePlString),
    DateTime(Vec<i64>, MutableBitmap), // nanoseconds
    AnyValue(Vec<AnyValue<'static>>),
    Empty,
}

impl TypedCol {
    pub fn new(dtype: &DataType, capacity: usize) -> Self {
        match dtype {
            DataType::Int64 => Self::Int64(Vec::with_capacity(capacity), MutableBitmap::with_capacity(capacity)),
            DataType::Float64 => Self::Float64(Vec::with_capacity(capacity), MutableBitmap::with_capacity(capacity)),
            DataType::Boolean => Self::Bool(Vec::with_capacity(capacity), MutableBitmap::with_capacity(capacity)),
            DataType::String => Self::String(MutablePlString::with_capacity(capacity)),
            DataType::Datetime(TimeUnit::Nanoseconds, None) => {
                Self::DateTime(Vec::with_capacity(capacity), MutableBitmap::with_capacity(capacity))
            }
            _ => Self::AnyValue(Vec::with_capacity(capacity)),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Int64(v, _) => v.is_empty(),
            Self::Float64(v, _) => v.is_empty(),
            Self::Bool(v, _) => v.is_empty(),
            Self::String(v) => v.len() == 0,
            Self::DateTime(v, _) => v.is_empty(),
            Self::AnyValue(v) => v.is_empty(),
            Self::Empty => true,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(v, _) => v.len(),
            Self::Float64(v, _) => v.len(),
            Self::Bool(v, _) => v.len(),
            Self::String(v) => v.len(),
            Self::DateTime(v, _) => v.len(),
            Self::AnyValue(v) => v.len(),
            Self::Empty => 0,
        }
    }

    /// 填充空值到目标长度（稀疏列补齐）
    pub fn pad_to(&mut self, target_len: usize) {
        match self {
            Self::Int64(vec, bitmap) => {
                while vec.len() < target_len {
                    vec.push(0);
                    bitmap.push(false);
                }
            }
            Self::Float64(vec, bitmap) => {
                while vec.len() < target_len {
                    vec.push(0.0);
                    bitmap.push(false);
                }
            }
            Self::Bool(vec, bitmap) => {
                while vec.len() < target_len {
                    vec.push(false);
                    bitmap.push(false);
                }
            }
            Self::String(arr) => {
                while arr.len() < target_len {
                    arr.push_null();
                }
            }
            Self::DateTime(vec, bitmap) => {
                while vec.len() < target_len {
                    vec.push(0);
                    bitmap.push(false);
                }
            }
            Self::AnyValue(vec) => {
                while vec.len() < target_len {
                    vec.push(AnyValue::Null);
                }
            }
            Self::Empty => {}
        }
    }

    /// 推入一个空值（用于稀疏补齐）
    pub fn push_null(&mut self) {
        match self {
            Self::Int64(v, b) => { v.push(0); b.push(false); }
            Self::Float64(v, b) => { v.push(0.0); b.push(false); }
            Self::Bool(v, b) => { v.push(false); b.push(false); }
            Self::String(arr) => { arr.push_null(); }
            Self::DateTime(v, b) => { v.push(0); b.push(false); }
            Self::AnyValue(v) => { v.push(AnyValue::Null); }
            Self::Empty => {}
        }
    }

    /// 判断当前列是否能直接容纳该 Data（无需升级）
    pub fn accepts(&self, data: &Data) -> bool {
        match (self, data) {
            (Self::Int64(_, _), Data::Int(_)) => true,
            (Self::Float64(_, _), Data::Float(_) | Data::Int(_)) => true,
            (Self::Bool(_, _), Data::Bool(_)) => true,
            (Self::String(_), Data::String(_) | Data::SharedStringRef(_) | Data::DateTimeIso(_) | Data::DurationIso(_)) => true,
            (Self::DateTime(_, _), Data::DateTime(_)) => true,
            (Self::AnyValue(_), _) => true,
            (_, Data::Empty | Data::Error(_)) => true,
            _ => false,
        }
    }

    /// 推入一个非空值（调用前必须保证 accepts() 为 true）
    pub fn push_value(&mut self, data: Data) {
        match self {
            Self::Int64(v, b) => {
                if let Data::Int(val) = data {
                    v.push(val);
                    b.push(true);
                }
            }
            Self::Float64(v, b) => {
                match data {
                    Data::Float(val) => { v.push(val); b.push(true); }
                    Data::Int(val) => { v.push(val as f64); b.push(true); }
                    _ => {}
                }
            }
            Self::Bool(v, b) => {
                if let Data::Bool(val) = data {
                    v.push(val);
                    b.push(true);
                }
            }
            Self::String(arr) => {
                let s = match data {
                    Data::String(s) => s,
                    Data::DateTimeIso(s) => s,
                    Data::DurationIso(s) => s,
                    _ => return,
                };
                arr.push_value(&s);
            }
            Self::DateTime(v, b) => {
                if let Data::DateTime(dt) = data {
                    v.push(dt.to_timestamp_nanos());
                    b.push(true);
                }
            }
            Self::AnyValue(v) => {
                v.push(data.into_anyvalue());
            }
            Self::Empty => {}
        }
    }

    /// 推入一个字符串值（绕过 Data 枚举，直接传 &str）
    pub fn push_str(&mut self, s: &str) {
        match self {
            Self::String(arr) => arr.push_value(s),
            Self::AnyValue(v) => v.push(AnyValue::StringOwned(PlSmallStr::from_str(s))),
            _ => {}
        }
    }

    /// 推入一个 shared string 引用，利用 Arrow StringView 直接引用外部 buffer，零拷贝。
    pub fn push_shared_string_ref(&mut self, idx: usize, strings: &SharedStrings) {
        match self {
            Self::String(arr) => {
                if let Some((offset, len)) = strings.offsets.get(idx) {
                    let offset_usize = *offset as usize;
                    let len_usize = *len as usize;
                    let slice = &strings.buffer[offset_usize..offset_usize + len_usize];
                    let view = View::new_from_bytes(slice, 0, *offset);
                    arr.push_view(view, std::slice::from_ref(&strings.buffer));
                } else {
                    arr.push_null();
                }
            }
            Self::AnyValue(v) => {
                if let Some((offset, len)) = strings.offsets.get(idx) {
                    let offset_usize = *offset as usize;
                    let len_usize = *len as usize;
                    let s = std::str::from_utf8(&strings.buffer[offset_usize..offset_usize + len_usize])
                        .unwrap_or_default();
                    v.push(AnyValue::StringOwned(PlSmallStr::from_str(s)));
                } else {
                    v.push(AnyValue::Null);
                }
            }
            _ => {}
        }
    }

    /// 类型升级（into_iter 转移所有权）
    pub fn upgrade(&mut self, target: &DataType) {
        let old = std::mem::replace(self, TypedCol::Empty);
        *self = match (old, target) {
            // Int64 → Float64
            (TypedCol::Int64(vec, bitmap), DataType::Float64) => {
                let new_vec: Vec<f64> = vec.into_iter().map(|v| v as f64).collect();
                TypedCol::Float64(new_vec, bitmap)
            }
            // Int64 → String
            (TypedCol::Int64(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) { arr.push_value(&v.to_string()); } else { arr.push_null(); }
                }
                TypedCol::String(arr)
            }
            // Float64 → String
            (TypedCol::Float64(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) { arr.push_value(&v.to_string()); } else { arr.push_null(); }
                }
                TypedCol::String(arr)
            }
            // Bool → String
            (TypedCol::Bool(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) { arr.push_value(&v.to_string()); } else { arr.push_null(); }
                }
                TypedCol::String(arr)
            }
            // DateTime → String
            (TypedCol::DateTime(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) { arr.push_value(&v.to_string()); } else { arr.push_null(); }
                }
                TypedCol::String(arr)
            }
            // 其他不兼容情况统一回退到 AnyValue
            (mut old, _) => {
                let mut av_vec = Vec::with_capacity(old.len());
                match &mut old {
                    TypedCol::Int64(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid { AnyValue::Int64(v) } else { AnyValue::Null });
                        }
                    }
                    TypedCol::Float64(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid { AnyValue::Float64(v) } else { AnyValue::Null });
                        }
                    }
                    TypedCol::Bool(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid { AnyValue::Boolean(v) } else { AnyValue::Null });
                        }
                    }
                    TypedCol::String(arr) => {
                        let frozen = std::mem::replace(arr, MutablePlString::with_capacity(0)).freeze();
                        for i in 0..frozen.len() {
                            if frozen.is_null(i) {
                                av_vec.push(AnyValue::Null);
                            } else {
                                av_vec.push(AnyValue::StringOwned(PlSmallStr::from(frozen.value(i))));
                            }
                        }
                    }
                    TypedCol::DateTime(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid { AnyValue::Datetime(v, TimeUnit::Nanoseconds, None) } else { AnyValue::Null });
                        }
                    }
                    TypedCol::AnyValue(vec) => {
                        std::mem::swap(&mut av_vec, vec);
                    }
                    TypedCol::Empty => {}
                }
                TypedCol::AnyValue(av_vec)
            }
        };
    }

    /// 转换为 Polars Series
    pub fn into_series(self, name: PlSmallStr, dtype: &DataType) -> PolarsResult<Series> {
        match (self, dtype) {
            (TypedCol::Int64(vec, bitmap), DataType::Int64) => {
                let bitmap: Bitmap = bitmap.into();
                Ok(Int64Chunked::from_vec_validity(name, vec, Some(bitmap)).into_series())
            }
            (TypedCol::Float64(vec, bitmap), DataType::Float64) => {
                let bitmap: Bitmap = bitmap.into();
                Ok(Float64Chunked::from_vec_validity(name, vec, Some(bitmap)).into_series())
            }
            (TypedCol::Bool(vec, bitmap), DataType::Boolean) => {
                let validity: Bitmap = bitmap.into();
                let values = Bitmap::from_iter(vec);
                let arr = polars_arrow::array::BooleanArray::new(
                    polars_arrow::datatypes::ArrowDataType::Boolean,
                    values,
                    Some(validity),
                );
                Ok(unsafe { BooleanChunked::from_chunks(name, vec![Box::new(arr)]) }.into_series())
            }
            (TypedCol::String(arr), DataType::String) => {
                let arr: Utf8ViewArray = arr.freeze();
                Ok(unsafe { StringChunked::from_chunks(name, vec![Box::new(arr)]) }.into_series())
            }
            (TypedCol::DateTime(vec, bitmap), DataType::Datetime(TimeUnit::Nanoseconds, None)) => {
                let bitmap: Bitmap = bitmap.into();
                Ok(Int64Chunked::from_vec_validity(name, vec, Some(bitmap))
                    .into_datetime(TimeUnit::Nanoseconds, None)
                    .into_series())
            }
            (TypedCol::AnyValue(vec), _) => {
                Series::from_any_values_and_dtype(name, &vec, dtype, false)
            }
            (col, _) => {
                // 类型不匹配时的降级处理：先转 AnyValue 再走老路
                let mut av_vec = Vec::with_capacity(col.len());
                match col {
                    TypedCol::Int64(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) { av_vec.push(AnyValue::Int64(v)); } else { av_vec.push(AnyValue::Null); }
                        }
                    }
                    TypedCol::Float64(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) { av_vec.push(AnyValue::Float64(v)); } else { av_vec.push(AnyValue::Null); }
                        }
                    }
                    TypedCol::Bool(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) { av_vec.push(AnyValue::Boolean(v)); } else { av_vec.push(AnyValue::Null); }
                        }
                    }
                    TypedCol::String(arr) => {
                        let frozen = arr.freeze();
                        for i in 0..frozen.len() {
                            if frozen.is_null(i) {
                                av_vec.push(AnyValue::Null);
                            } else {
                                av_vec.push(AnyValue::StringOwned(PlSmallStr::from(frozen.value(i))));
                            }
                        }
                    }
                    TypedCol::DateTime(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) { av_vec.push(AnyValue::Datetime(v, TimeUnit::Nanoseconds, None)); } else { av_vec.push(AnyValue::Null); }
                        }
                    }
                    _ => {}
                }
                Series::from_any_values_and_dtype(name, &av_vec, dtype, false)
            }
        }
    }
}

// 辅助 trait：将 Data 转为 AnyValue（保留给 AnyValue 回退列和 header 解析使用）
pub trait IntoAnyValue {
    fn into_anyvalue(self) -> AnyValue<'static>;
}

impl IntoAnyValue for Data {
    fn into_anyvalue(self) -> AnyValue<'static> {
        match self {
            Data::Int(v) => AnyValue::Int64(v),
            Data::Float(v) => AnyValue::Float64(v),
            Data::Bool(v) => AnyValue::Boolean(v),
            Data::String(v) => AnyValue::StringOwned(v),
            Data::DateTime(v) => AnyValue::Datetime(v.to_timestamp_nanos(), TimeUnit::Nanoseconds, None),
            Data::DateTimeIso(v) => AnyValue::StringOwned(v),
            Data::DurationIso(v) => AnyValue::StringOwned(v),
            Data::SharedStringRef(idx) => AnyValue::StringOwned(PlSmallStr::from_string(idx.to_string())),
            Data::Error(_) | Data::Empty => AnyValue::Null,
        }
    }
}

// 保留 FromData trait（header 解析等场景仍需要）
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
        data.into_anyvalue()
    }
}

impl FromData for String {
    fn from_data(data: Data) -> Self {
        match data {
            Data::String(s) => s.to_string(),
            Data::Int(i) => i.to_string(),
            Data::Float(f) => f.to_string(),
            Data::Bool(b) => b.to_string(),
            Data::DateTime(dt) => dt.to_string(),
            Data::DateTimeIso(s) | Data::DurationIso(s) => s.to_string(),
            Data::Error(e) => e.to_string(),
            Data::SharedStringRef(idx) => idx.to_string(),
            Data::Empty => String::new(),
        }
    }
}

/// 流式读取所需要的多个列
#[derive(Debug)]
pub struct TypedCols {
    pub cols: Vec<TypedCol>,
    pub batch_size: usize,
    pub headers: Vec<String>,
    pub col_dtypes: Vec<Option<DataType>>,
    pub strings: Option<Arc<SharedStrings>>,
}

fn data_to_dtype(data: &Data) -> DataType {
    match data {
        Data::Int(_) => DataType::Int64,
        Data::Float(_) => DataType::Float64,
        Data::Bool(_) => DataType::Boolean,
        Data::String(_) | Data::SharedStringRef(_) | Data::DateTimeIso(_) | Data::DurationIso(_) => DataType::String,
        Data::DateTime(_) => DataType::Datetime(TimeUnit::Nanoseconds, None),
        Data::Error(_) | Data::Empty => DataType::Null,
    }
}

impl TypedCols {
    pub fn new(dimension: &Dimensions, batch_size: usize) -> Self {
        let col_num = dimension.end.1 as usize + 1;
        Self {
            cols: (0..col_num).map(|_| TypedCol::Empty).collect(),
            batch_size,
            headers: Vec::with_capacity(col_num),
            col_dtypes: vec![None; col_num],
            strings: None,
        }
    }

    pub fn push_cell(&mut self, cell: Cell<Data>, batch_row: usize) -> anyhow::Result<()> {
        let (_, y) = cell.get_position();
        let y = y as usize;

        // 动态扩展列
        if y >= self.cols.len() {
            let start = self.cols.len();
            for _ in start..=y {
                self.cols.push(TypedCol::Empty);
                self.col_dtypes.push(None);
            }
        }

        let data = cell.into_value();
        let is_null = matches!(data, Data::Empty | Data::Error(_));

        // 推断类型
        self.infer_dtype(y, &data);
        let target_dtype: &Option<DataType> = &self.col_dtypes[y];

        // 初始化空列
        if matches!(self.cols[y], TypedCol::Empty) {
            let dtype = target_dtype.as_ref().unwrap_or(&DataType::Null);
            if is_null && *dtype == DataType::Null {
                // 全是 null 且类型未知，先用 AnyValue 占位
                self.cols[y] = TypedCol::AnyValue(Vec::with_capacity(self.batch_size));
            } else {
                self.cols[y] = TypedCol::new(dtype, self.batch_size);
            }
        }

        // 稀疏补齐
        let current_len = self.cols[y].len();
        let empty_num = batch_row.saturating_sub(current_len);
        for _ in 0..empty_num {
            self.cols[y].push_null();
        }

        // 类型升级检查
        if !is_null && !self.cols[y].accepts(&data) {
            if let Some(dtype) = target_dtype {
                self.cols[y].upgrade(dtype);
            } else {
                self.cols[y].upgrade(&DataType::String);
            }
        }

        // 推入值
        if is_null {
            self.cols[y].push_null();
        } else if let Data::SharedStringRef(idx) = &data {
            if let Some(strings) = &self.strings {
                self.cols[y].push_shared_string_ref(*idx, strings);
            } else {
                self.cols[y].push_null();
            }
        } else {
            self.cols[y].push_value(data);
        }

        Ok(())
    }

    fn infer_dtype(&mut self, col_idx: usize, data: &Data) {
        if matches!(data, Data::Empty | Data::Error(_)) {
            return;
        }
        let new_dtype = data_to_dtype(data);
        let current = &mut self.col_dtypes[col_idx];
        *current = match (current.take(), new_dtype) {
            (None, dt) => Some(dt),
            (Some(DataType::Int64), DataType::Float64)
            | (Some(DataType::Float64), DataType::Int64) => Some(DataType::Float64),
            (Some(dt1), dt2) if dt1 == dt2 => Some(dt1),
            _ => Some(DataType::String),
        };
    }

    pub fn into_dataframe(&mut self) -> PolarsResult<DataFrame> {
        let max_len = self.cols.iter().map(|c| c.len()).max().unwrap_or(0);
        let _num_cols = self.cols.len();

        // 补齐所有列到 max_len
        for col in &mut self.cols {
            col.pad_to(max_len);
        }

        let columns: Vec<Column> = std::mem::take(&mut self.cols)
            .into_iter()
            .enumerate()
            .map(|(i, col)| {
                let name = self.headers.get(i).map(|s| s.as_str()).unwrap_or("unknown");
                let dtype = self.col_dtypes.get(i).and_then(|d| d.as_ref());
                let series = if let Some(dt) = dtype {
                    col.into_series(name.into(), dt)?
                } else {
                    col.into_series(name.into(), &DataType::Null)?
                };
                Ok::<_, polars::error::PolarsError>(series.into())
            })
            .collect::<Result<Vec<_>, _>>()?;

        DataFrame::new_infer_height(columns)
    }
}

/// 将单元格 Data 解析为 header 字符串，shared string 会查表解析为实际内容。
fn cell_value_to_header(data: Data, strings: Option<&Arc<SharedStrings>>) -> String {
    match data {
        Data::SharedStringRef(idx) => {
            if let Some(s) = strings {
                if let Some((offset, len)) = s.offsets.get(idx) {
                    let start = *offset as usize;
                    let end = start + *len as usize;
                    return std::str::from_utf8(&s.buffer[start..end])
                        .unwrap_or_default()
                        .to_string();
                }
            }
            String::new()
        }
        other => other.into(),
    }
}

/// 统一两种 sheet reader 的枚举，避免 trait object 的虚函数开销。
enum SheetReader {
    Stream(XlsxStreamReader),
    Fast(SheetFastReader),
}

impl SheetReader {
    fn next_cell(&mut self) -> anyhow::Result<Option<Cell<Data>>> {
        match self {
            Self::Stream(r) => r.next_cell(),
            Self::Fast(r) => r.next_cell(),
        }
    }

    fn dimensions(&self) -> Dimensions {
        match self {
            Self::Stream(r) => r.dimensions(),
            Self::Fast(r) => r.dimensions(),
        }
    }

    fn strings(&self) -> &Arc<SharedStrings> {
        match self {
            Self::Stream(r) => r.strings(),
            Self::Fast(r) => r.strings(),
        }
    }
}

/// 流式 xlsx DataFrame 迭代器。
///
/// 底层根据 fast 标志选择 `XlsxStreamReader`（单线程流式）或
/// `SheetFastReader`（并发解析），不依赖 calamine。
pub struct DataFrameIter {
    workbook: Arc<XlsxWorkbook>,
    reader: SheetReader,
    fast: bool,
    cols: TypedCols,
    cell_cache: Option<Cell<Data>>,
    has_header: bool,
    len: usize,                      // 总批次数
    batch_start_row: Option<u32>,    // 当前批次的起始绝对行号
    current_row_count: usize,        // 当前批次已收集的行数（用于批次截断）
    last_processed_row: Option<u32>, // 上一个处理的绝对行号（检测行切换)
    current_sheet_name: Option<String>,
    current_sheet_idx: Option<usize>,
    skip_rows_sorted: Vec<u32>,      // 排序后的 skip 行列表（单调递增游标查询）
    skip_rows_idx: usize,            // skip_rows_sorted 的当前游标
    current_row_skipped: bool,       // 缓存当前行是否被跳过
}

impl DataFrameIter {
    pub fn new<P>(
        batch_size: Option<usize>,
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        fast: bool,
    ) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        let workbook = if fast {
            Arc::new(XlsxWorkbook::open_fast(path)?)
        } else {
            Arc::new(XlsxWorkbook::open(path)?)
        };
        Self::from_workbook(batch_size, workbook, sheet_name, sheet_idx, has_header, skip_rows, fast)
    }

    pub fn from_workbook(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        fast: bool,
    ) -> anyhow::Result<Self> {
        let reader = if fast {
            SheetReader::Fast(SheetFastReader::new(
                workbook.path(), sheet_name, sheet_idx,
            )?)
        } else {
            SheetReader::Stream(XlsxStreamReader::from_workbook(
                Arc::clone(&workbook), sheet_name, sheet_idx,
            )?)
        };
        let dim = reader.dimensions();
        let batch_size = match batch_size {
            Some(s) => s,
            None => dim.end.0 as usize + if has_header { 0 } else { 1 },
        };
        let mut cols = TypedCols::new(&dim, batch_size);
        cols.strings = Some(Arc::clone(reader.strings()));
        let mut skip_rows_sorted: Vec<u32> = skip_rows.map(|s| s.iter().copied().collect()).unwrap_or_default();
        skip_rows_sorted.sort_unstable();
        let mut iter = Self {
            workbook,
            reader,
            cols,
            cell_cache: None,
            has_header,
            fast,
            len: 0,
            batch_start_row: None,
            current_row_count: 0,
            last_processed_row: None,
            current_sheet_name: sheet_name.map(|s| s.to_string()),
            current_sheet_idx: sheet_idx,
            skip_rows_sorted,
            skip_rows_idx: 0,
            current_row_skipped: false,
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
        self.reader = if self.fast {
            SheetReader::Fast(SheetFastReader::new(
                self.workbook.path(), sheet_name, sheet_idx,
            )?)
        } else {
            SheetReader::Stream(XlsxStreamReader::from_workbook(
                Arc::clone(&self.workbook), sheet_name, sheet_idx,
            )?)
        };
        let dim = self.reader.dimensions();
        let batch_size = self.cols.batch_size;
        self.cols = TypedCols::new(&dim, batch_size);
        self.cols.strings = Some(Arc::clone(self.reader.strings()));
        self.cell_cache = None;
        self.batch_start_row = None;
        self.current_row_count = 0;
        self.last_processed_row = None;
        self.current_sheet_name = sheet_name.map(|s| s.to_string());
        self.current_sheet_idx = sheet_idx;
        self.len = 0;
        self.skip_rows_idx = 0;
        self.current_row_skipped = false;
        self.find_header(batch_size)?;
        Ok(())
    }

    fn find_header(&mut self, batch_size: usize) -> anyhow::Result<()> {
        let strings = self.cols.strings.clone();
        let first_cell = match self.reader.next_cell()? {
            Some(cell) => cell,
            None => {
                self.cols
                    .cols
                    .iter()
                    .enumerate()
                    .for_each(|(i, _)| self.cols.headers.push(format!("col_{}", i)));
                self.len = 0;
                return Ok(());
            }
        };
        let first_x = first_cell.get_position().0;
        let mut total_rows: usize;
        if self.has_header {
            self.cols.headers.push(cell_value_to_header(first_cell.into_value(), strings.as_ref()));
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
                            let mut value = cell_value_to_header(cell.into_value(), strings.as_ref());
                            value = if value.is_empty() {
                                format!("Unknown_{}", y)
                            } else {
                                value
                            };
                            self.cols.headers.push(value);
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
                .cols
                .iter()
                .enumerate()
                .for_each(|(i, _)| self.cols.headers.push(format!("col_{}", i)));
            total_rows = (self.reader.dimensions().end.0 - first_x + 1) as usize;
        }
        let skip_count = self.skip_rows_sorted.iter()
            .filter(|&&r| r >= first_x && r <= self.reader.dimensions().end.0)
            .count();
        total_rows = total_rows.saturating_sub(skip_count);
        self.len = (total_rows + batch_size - 1) / batch_size;
        Ok(())
    }

    /// 用单调递增游标判断某行是否需要跳过（要求 row 按非递减顺序调用）
    fn is_row_skipped(&mut self, row: u32) -> bool {
        if self.skip_rows_sorted.is_empty() {
            return false;
        }
        while self.skip_rows_idx < self.skip_rows_sorted.len()
            && self.skip_rows_sorted[self.skip_rows_idx] < row
        {
            self.skip_rows_idx += 1;
        }
        self.skip_rows_idx < self.skip_rows_sorted.len()
            && self.skip_rows_sorted[self.skip_rows_idx] == row
    }

    fn finish_batch(&mut self) -> Option<anyhow::Result<DataFrame>> {
        let has_data = self.cols.cols.iter().any(|c| !c.is_empty());
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
        // 获取第一个非跳过的 cell 作为 batch 起点
        if self.batch_start_row.is_none() {
            loop {
                let cell = if let Some(c) = self.cell_cache.take() {
                    c
                } else {
                    match self.reader.next_cell() {
                        Ok(Some(c)) => c,
                        Ok(None) => return None,
                        Err(e) => return Some(Err(e)),
                    }
                };
                let row = cell.get_position().0;
                self.current_row_skipped = self.is_row_skipped(row);
                if self.current_row_skipped {
                    continue;
                }
                self.batch_start_row = Some(row);
                self.current_row_count = 1;
                self.last_processed_row = Some(row);
                if let Err(e) = self.cols.push_cell(cell, 0) {
                    return Some(Err(e));
                }
                break;
            }
        }

        loop {
            match self.reader.next_cell() {
                Ok(Some(cell)) => {
                    let current_row = cell.get_position().0;

                    if self.last_processed_row.map_or(true, |lr| lr != current_row) {
                        // 行切换：更新 skip 状态
                        self.current_row_skipped = self.is_row_skipped(current_row);
                        if self.current_row_skipped {
                            continue;
                        }
                        if self.current_row_count >= self.cols.batch_size {
                            self.cell_cache = Some(cell);
                            self.len = self.len.saturating_sub(1);
                            return self.finish_batch();
                        }
                        self.current_row_count += 1;
                        self.last_processed_row = Some(current_row);
                    } else if self.current_row_skipped {
                        // 同一行复用 skip 状态
                        continue;
                    }

                    let batch_row = self.current_row_count.saturating_sub(1) as usize;
                    if let Err(e) = self.cols.push_cell(cell, batch_row) {
                        return Some(Err(e));
                    }
                }
                Ok(None) => {
                    let has_data = self.cols.cols.iter().any(|c| !c.is_empty());
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
/// Low-memory mode (default): stream sharedStrings.xml via quick-xml.
pub fn df_iter(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
) -> anyhow::Result<DataFrameIter> {
    DataFrameIter::new(batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, false)
}

/// Fast mode: fully decompress sharedStrings.xml then byte-scan.
/// Trades ~2-4GB extra peak memory for ~1.5x faster init().
pub fn df_iter_fast(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
) -> anyhow::Result<DataFrameIter> {
    DataFrameIter::new(batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, true)
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
        let iter = df_iter(10.into(), &path, "Sheet1".into(), None, true, None)?;
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
        let mut iter = DataFrameIter::from_workbook(Some(5), Arc::clone(&wb), Some("Sheet1"), None, true, None, false)?;

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

#[cfg(test)]
mod skip_rows_tests {
    use super::*;

    #[test]
    fn test_skip_rows() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        // skip row 1 (second data row, 0-based)
        let iter = df_iter(Some(100), &path, Some("Sheet1"), None, true, Some(&[1]))?;
        let mut total_rows = 0;
        for batch in iter {
            let df = batch?;
            total_rows += df.height();
        }
        // Without skip: test_data has header + 99999 data rows = 100000 total cells / 7 cols ≈ 14286 rows
        // With skip row 1: one less data row
        println!("total rows with skip: {}", total_rows);
        assert!(total_rows > 0);
        Ok(())
    }

    #[test]
    fn test_skip_rows_batch_boundary() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        // batch_size=5, skip row 1: row 1 is skipped before batch starts,
        // so first batch reads raw rows 2,3,4,5,6 → outputs 5 rows
        let iter = df_iter(Some(5), &path, Some("Sheet1"), None, true, Some(&[1]))?;
        for (i, batch) in iter.enumerate() {
            let df = batch?;
            println!("batch {}: {} rows", i, df.height());
            if i == 0 {
                assert_eq!(df.height(), 5, "first batch should have 5 rows (row 1 skipped before batch)");
            }
            break;
        }
        Ok(())
    }

    #[test]
    fn test_skip_rows_within_batch() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        // batch_size=5, skip rows 2,3,4: within batch, 3 rows skipped → output 2 rows
        let iter = df_iter(Some(5), &path, Some("Sheet1"), None, true, Some(&[2, 3, 4]))?;
        for (i, batch) in iter.enumerate() {
            let df = batch?;
            println!("batch {}: {} rows", i, df.height());
            if i == 0 {
                // Batch reads raw rows 1,2,3,4,5,6,7,8 (needs 5 valid rows)
                // Skip 2,3,4. Valid: 1,5,6,7,8 → 5 rows
                assert_eq!(df.height(), 5);
            }
            break;
        }
        Ok(())
    }
}

#[cfg(test)]
mod skip_header_interaction_tests {
    use super::*;

    #[test]
    fn test_header_not_affected_by_skip() -> anyhow::Result<()> {
        // Scenario 1: skip_rows=[1], header=row0
        // Header should be read correctly, row1 skipped
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let iter = df_iter(Some(5), &path, Some("Sheet1"), None, true, Some(&[1]))?;
        let df = iter.into_iter().next().unwrap()?;
        println!("Scenario 1 headers: {:?}", df.get_column_names());
        println!("Scenario 1 first row: {:?}", df.get_row(0));
        assert!(df.height() > 0);
        Ok(())
    }

    #[test]
    fn test_skip_header_row_with_has_header_true() -> anyhow::Result<()> {
        // Scenario 2: skip_rows=[0], has_header=true
        // Row 0 becomes header (current behavior)
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let iter = df_iter(Some(5), &path, Some("Sheet1"), None, true, Some(&[0]))?;
        let df = iter.into_iter().next().unwrap()?;
        println!("Scenario 2 headers: {:?}", df.get_column_names());
        println!("Scenario 2 first row: {:?}", df.get_row(0));
        // Headers are the content of row 0
        Ok(())
    }

    #[test]
    fn test_skip_first_data_row_no_header() -> anyhow::Result<()> {
        // Scenario 3: has_header=false, skip_rows=[0]
        // First data row (row 0) should be skipped
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let iter = df_iter(Some(5), &path, Some("Sheet1"), None, false, Some(&[0]))?;
        let df = iter.into_iter().next().unwrap()?;
        println!("Scenario 3 headers: {:?}", df.get_column_names());
        println!("Scenario 3 first row: {:?}", df.get_row(0));
        assert!(df.height() > 0);
        Ok(())
    }

    #[test]
    fn test_skip_second_row_no_header() -> anyhow::Result<()> {
        // Scenario 4: has_header=false, skip_rows=[1]
        // Row 0 is data, row 1 is skipped
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let iter = df_iter(Some(5), &path, Some("Sheet1"), None, false, Some(&[1]))?;
        let df = iter.into_iter().next().unwrap()?;
        println!("Scenario 4 headers: {:?}", df.get_column_names());
        println!("Scenario 4 first row: {:?}", df.get_row(0));
        assert!(df.height() > 0);
        Ok(())
    }
}

#[cfg(test)]
mod zero_copy_tests {
    use super::*;
    use polars_buffer::Buffer;

    #[test]
    fn test_push_shared_string_ref_zero_copy() {
        let strings = SharedStrings {
            buffer: Buffer::from_vec(b"hello world foo bar".to_vec()),
            offsets: vec![(0, 5), (6, 5), (12, 3), (16, 3)],
        };
        let mut col = TypedCol::String(MutablePlString::with_capacity(4));
        
        col.push_shared_string_ref(0, &strings); // "hello" (5 bytes) -> inline
        col.push_shared_string_ref(1, &strings); // "world" (5 bytes) -> inline
        col.push_shared_string_ref(2, &strings); // "foo" (3 bytes) -> inline
        col.push_shared_string_ref(3, &strings); // "bar" (3 bytes) -> inline
        
        // 所有字符串 <=12 bytes，应该全部被 inline，不引用外部 buffer
        if let TypedCol::String(arr) = &col {
            assert_eq!(arr.len(), 4);
            // 因为没有非 inline 字符串，completed_buffers 应该为空
            assert!(arr.completed_buffers().is_empty());
            // in_progress_buffer 也应该为空（inline 不写入 buffer）
            assert_eq!(arr.total_buffer_len(), 0);
        } else {
            panic!("expected String col");
        }
    }

    #[test]
    fn test_push_shared_string_ref_long_zero_copy() {
        let long_str = "a".repeat(100);
        let mut buffer = Vec::new();
        buffer.extend_from_slice(long_str.as_bytes());
        let strings = SharedStrings {
            buffer: Buffer::from_vec(buffer),
            offsets: vec![(0, 100)],
        };
        let mut col = TypedCol::String(MutablePlString::with_capacity(1));
        
        col.push_shared_string_ref(0, &strings); // 100 bytes -> non-inline, 引用外部 buffer
        
        if let TypedCol::String(arr) = &col {
            assert_eq!(arr.len(), 1);
            // 应该引用外部 buffer，completed_buffers 里应该有 1 个 buffer
            assert_eq!(arr.completed_buffers().len(), 1);
            // total_buffer_len 应该等于 100
            assert_eq!(arr.total_buffer_len(), 100);
        } else {
            panic!("expected String col");
        }
    }
}
