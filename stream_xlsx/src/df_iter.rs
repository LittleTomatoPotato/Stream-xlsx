use crate::{
    excel_types::{Cell, Data, Dimensions},
    sheet_fast::SheetFastReader,
    workbook::{SharedStrings, XlsxWorkbook},
    xlsx_stream_lm::XlsxStreamReader,
};
use polars::prelude::*;
use polars_arrow::array::{Array, MutablePlString, Utf8ViewArray, View};
use polars_arrow::bitmap::{Bitmap, MutableBitmap};
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashSet},
    iter::FusedIterator,
    path::Path,
    sync::Arc,
};

// ------------------------------------------------------------------
// TypedCol / TypedCols：按类型存储列数据，数值列实现零拷贝构建
// ------------------------------------------------------------------

#[derive(Debug)]
pub enum TypedCol {
    Int64(Vec<i64>, MutableBitmap),
    Float64(Vec<f64>, MutableBitmap),
    Decimal(Vec<i128>, MutableBitmap, usize, usize),
    Bool(Vec<bool>, MutableBitmap),
    String(MutablePlString),
    Date(Vec<i32>, MutableBitmap),     // Unix epoch 起算的天数
    DateTime(Vec<i64>, MutableBitmap), // nanoseconds
    AnyValue(Vec<AnyValue<'static>>),
    Null(usize),
    Empty,
}

impl TypedCol {
    pub fn new(dtype: &DataType, capacity: usize) -> Self {
        match dtype {
            DataType::Int64 => Self::Int64(
                Vec::with_capacity(capacity),
                MutableBitmap::with_capacity(capacity),
            ),
            DataType::Float64 => Self::Float64(
                Vec::with_capacity(capacity),
                MutableBitmap::with_capacity(capacity),
            ),
            DataType::Boolean => Self::Bool(
                Vec::with_capacity(capacity),
                MutableBitmap::with_capacity(capacity),
            ),
            DataType::String => Self::String(MutablePlString::with_capacity(capacity)),
            DataType::Date => Self::Date(
                Vec::with_capacity(capacity),
                MutableBitmap::with_capacity(capacity),
            ),
            DataType::Datetime(_, None) => Self::DateTime(
                Vec::with_capacity(capacity),
                MutableBitmap::with_capacity(capacity),
            ),
            DataType::Null => Self::Null(0),
            _ => Self::AnyValue(Vec::with_capacity(capacity)),
        }
    }

    /// 固定 Schema 热路径仅接受能够直接写入原生 Arrow builder 的类型。
    /// 不支持的类型在读取数据前报错，避免静默退化到 AnyValue。
    fn new_strict(dtype: &DataType, capacity: usize) -> anyhow::Result<Self> {
        match dtype {
            DataType::Decimal(precision, scale)
                if (1..=38).contains(precision) && scale <= precision =>
            {
                Ok(Self::Decimal(
                    Vec::with_capacity(capacity),
                    MutableBitmap::with_capacity(capacity),
                    *precision,
                    *scale,
                ))
            }
            DataType::Int64
            | DataType::Float64
            | DataType::Boolean
            | DataType::String
            | DataType::Date
            | DataType::Datetime(_, None)
            | DataType::Null => Ok(Self::new(dtype, capacity)),
            DataType::Datetime(_, Some(_)) => Err(anyhow::anyhow!(
                "严格读取暂不支持带时区的 Datetime；XLSX 日期时间本身不携带时区"
            )),
            other => Err(anyhow::anyhow!(
                "严格读取暂不支持 Polars 类型 {other:?}；支持 Int64、Float64、Decimal、Boolean、String、Date、无时区 Datetime 和 Null"
            )),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Int64(v, _) => v.is_empty(),
            Self::Float64(v, _) => v.is_empty(),
            Self::Decimal(v, _, _, _) => v.is_empty(),
            Self::Bool(v, _) => v.is_empty(),
            Self::String(v) => v.len() == 0,
            Self::Date(v, _) => v.is_empty(),
            Self::DateTime(v, _) => v.is_empty(),
            Self::AnyValue(v) => v.is_empty(),
            Self::Null(len) => *len == 0,
            Self::Empty => true,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(v, _) => v.len(),
            Self::Float64(v, _) => v.len(),
            Self::Decimal(v, _, _, _) => v.len(),
            Self::Bool(v, _) => v.len(),
            Self::String(v) => v.len(),
            Self::Date(v, _) => v.len(),
            Self::DateTime(v, _) => v.len(),
            Self::AnyValue(v) => v.len(),
            Self::Null(len) => *len,
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
            Self::Decimal(vec, bitmap, _, _) => {
                while vec.len() < target_len {
                    vec.push(0);
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
            Self::Date(vec, bitmap) => {
                while vec.len() < target_len {
                    vec.push(0);
                    bitmap.push(false);
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
            Self::Null(len) => *len = (*len).max(target_len),
            Self::Empty => {}
        }
    }

    /// 推入一个空值（用于稀疏补齐）
    pub fn push_null(&mut self) {
        match self {
            Self::Int64(v, b) => {
                v.push(0);
                b.push(false);
            }
            Self::Float64(v, b) => {
                v.push(0.0);
                b.push(false);
            }
            Self::Decimal(v, b, _, _) => {
                v.push(0);
                b.push(false);
            }
            Self::Bool(v, b) => {
                v.push(false);
                b.push(false);
            }
            Self::String(arr) => {
                arr.push_null();
            }
            Self::Date(v, b) => {
                v.push(0);
                b.push(false);
            }
            Self::DateTime(v, b) => {
                v.push(0);
                b.push(false);
            }
            Self::AnyValue(v) => {
                v.push(AnyValue::Null);
            }
            Self::Null(len) => *len += 1,
            Self::Empty => {}
        }
    }

    /// 判断当前列是否能直接容纳该 Data（无需升级）
    pub fn accepts(&self, data: &Data) -> bool {
        match (self, data) {
            (Self::Int64(_, _), Data::Int(_)) => true,
            (Self::Float64(_, _), Data::Float(_) | Data::Int(_)) => true,
            (Self::Bool(_, _), Data::Bool(_)) => true,
            (
                Self::String(_),
                Data::String(_)
                | Data::SharedStringRef(_)
                | Data::DateTimeIso(_)
                | Data::DurationIso(_),
            ) => true,
            (Self::Date(_, _), Data::DateTime(_)) => true,
            (Self::DateTime(_, _), Data::DateTime(_)) => true,
            (Self::AnyValue(_), _) => true,
            (Self::Null(_), Data::Empty | Data::Error(_)) => true,
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
            Self::Float64(v, b) => match data {
                Data::Float(val) => {
                    v.push(val);
                    b.push(true);
                }
                Data::Int(val) => {
                    v.push(val as f64);
                    b.push(true);
                }
                _ => {}
            },
            Self::Decimal(_, _, _, _) => {
                unreachable!("Decimal 只能从原始数值词法经固定 Schema 严格路径写入")
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
            Self::Date(v, b) => {
                if let Data::DateTime(dt) = data {
                    const NANOS_PER_DAY: i64 = 86_400_000_000_000;
                    v.push(dt.to_timestamp_nanos().div_euclid(NANOS_PER_DAY) as i32);
                    b.push(true);
                }
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
            Self::Null(_) => {}
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
        let _ = self.push_shared_string_ref_checked(idx, strings);
    }

    /// 与 `push_shared_string_ref` 相同，但返回索引是否有效，供严格模式报错。
    fn push_shared_string_ref_checked(&mut self, idx: usize, strings: &SharedStrings) -> bool {
        match self {
            Self::String(arr) => {
                if let Some((offset, len)) = strings.offsets.get(idx) {
                    let offset_usize = *offset as usize;
                    let len_usize = *len as usize;
                    let slice = &strings.buffer[offset_usize..offset_usize + len_usize];
                    let view = View::new_from_bytes(slice, 0, *offset);
                    arr.push_view(view, std::slice::from_ref(&strings.buffer));
                    true
                } else {
                    arr.push_null();
                    false
                }
            }
            Self::AnyValue(v) => {
                if let Some((offset, len)) = strings.offsets.get(idx) {
                    let offset_usize = *offset as usize;
                    let len_usize = *len as usize;
                    let s = std::str::from_utf8(
                        &strings.buffer[offset_usize..offset_usize + len_usize],
                    )
                    .unwrap_or_default();
                    v.push(AnyValue::StringOwned(PlSmallStr::from_str(s)));
                    true
                } else {
                    v.push(AnyValue::Null);
                    false
                }
            }
            _ => false,
        }
    }

    fn matches_exact_dtype(&self, dtype: &DataType) -> bool {
        if let (Self::Decimal(_, _, precision, scale), DataType::Decimal(expected_p, expected_s)) =
            (self, dtype)
        {
            return precision == expected_p && scale == expected_s;
        }
        matches!(
            (self, dtype),
            (Self::Int64(_, _), DataType::Int64)
                | (Self::Float64(_, _), DataType::Float64)
                | (Self::Bool(_, _), DataType::Boolean)
                | (Self::String(_), DataType::String)
                | (Self::Date(_, _), DataType::Date)
                | (Self::DateTime(_, _), DataType::Datetime(_, None))
                | (Self::Null(_), DataType::Null)
        )
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
                    if bitmap.get(i) {
                        arr.push_value(&v.to_string());
                    } else {
                        arr.push_null();
                    }
                }
                TypedCol::String(arr)
            }
            // Float64 → String
            (TypedCol::Float64(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) {
                        arr.push_value(&v.to_string());
                    } else {
                        arr.push_null();
                    }
                }
                TypedCol::String(arr)
            }
            // Bool → String
            (TypedCol::Bool(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) {
                        arr.push_value(&v.to_string());
                    } else {
                        arr.push_null();
                    }
                }
                TypedCol::String(arr)
            }
            // DateTime → String
            (TypedCol::DateTime(vec, bitmap), DataType::String) => {
                let mut arr = MutablePlString::with_capacity(bitmap.len());
                for (i, v) in vec.into_iter().enumerate() {
                    if bitmap.get(i) {
                        arr.push_value(&v.to_string());
                    } else {
                        arr.push_null();
                    }
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
                            av_vec.push(if valid {
                                AnyValue::Int64(v)
                            } else {
                                AnyValue::Null
                            });
                        }
                    }
                    TypedCol::Float64(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid {
                                AnyValue::Float64(v)
                            } else {
                                AnyValue::Null
                            });
                        }
                    }
                    TypedCol::Decimal(_, _, _, _) => {
                        unreachable!("Decimal builder 仅在固定 Schema 严格读取中使用")
                    }
                    TypedCol::Bool(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid {
                                AnyValue::Boolean(v)
                            } else {
                                AnyValue::Null
                            });
                        }
                    }
                    TypedCol::String(arr) => {
                        let frozen =
                            std::mem::replace(arr, MutablePlString::with_capacity(0)).freeze();
                        for i in 0..frozen.len() {
                            if frozen.is_null(i) {
                                av_vec.push(AnyValue::Null);
                            } else {
                                av_vec
                                    .push(AnyValue::StringOwned(PlSmallStr::from(frozen.value(i))));
                            }
                        }
                    }
                    TypedCol::Date(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid {
                                AnyValue::Date(v)
                            } else {
                                AnyValue::Null
                            });
                        }
                    }
                    TypedCol::DateTime(vec, bitmap) => {
                        for (i, v) in vec.drain(..).enumerate() {
                            let valid = bitmap.get(i);
                            av_vec.push(if valid {
                                AnyValue::Datetime(v, TimeUnit::Nanoseconds, None)
                            } else {
                                AnyValue::Null
                            });
                        }
                    }
                    TypedCol::AnyValue(vec) => {
                        std::mem::swap(&mut av_vec, vec);
                    }
                    TypedCol::Null(len) => {
                        av_vec.resize(*len, AnyValue::Null);
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
            (
                TypedCol::Decimal(vec, bitmap, precision, scale),
                DataType::Decimal(expected_p, expected_s),
            ) if precision == *expected_p && scale == *expected_s => {
                let bitmap: Bitmap = bitmap.into();
                Ok(Int128Chunked::from_vec_validity(name, vec, Some(bitmap))
                    .into_decimal(precision, scale)?
                    .into_series())
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
            (TypedCol::Date(vec, bitmap), DataType::Date) => {
                let bitmap: Bitmap = bitmap.into();
                Ok(Int32Chunked::from_vec_validity(name, vec, Some(bitmap))
                    .into_date()
                    .into_series())
            }
            (TypedCol::DateTime(vec, bitmap), DataType::Datetime(time_unit, time_zone)) => {
                let bitmap: Bitmap = bitmap.into();
                Ok(Int64Chunked::from_vec_validity(name, vec, Some(bitmap))
                    .into_datetime(*time_unit, time_zone.clone())
                    .into_series())
            }
            (TypedCol::Null(len), DataType::Null) => Ok(Series::new_null(name, len)),
            (TypedCol::AnyValue(vec), _) => {
                Series::from_any_values_and_dtype(name, &vec, dtype, false)
            }
            (col, _) => {
                // 类型不匹配时的降级处理：先转 AnyValue 再走老路
                let mut av_vec = Vec::with_capacity(col.len());
                match col {
                    TypedCol::Int64(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) {
                                av_vec.push(AnyValue::Int64(v));
                            } else {
                                av_vec.push(AnyValue::Null);
                            }
                        }
                    }
                    TypedCol::Float64(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) {
                                av_vec.push(AnyValue::Float64(v));
                            } else {
                                av_vec.push(AnyValue::Null);
                            }
                        }
                    }
                    TypedCol::Decimal(_, _, _, _) => {
                        unreachable!("Decimal builder 与固定 Schema 类型不匹配")
                    }
                    TypedCol::Bool(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) {
                                av_vec.push(AnyValue::Boolean(v));
                            } else {
                                av_vec.push(AnyValue::Null);
                            }
                        }
                    }
                    TypedCol::String(arr) => {
                        let frozen = arr.freeze();
                        for i in 0..frozen.len() {
                            if frozen.is_null(i) {
                                av_vec.push(AnyValue::Null);
                            } else {
                                av_vec
                                    .push(AnyValue::StringOwned(PlSmallStr::from(frozen.value(i))));
                            }
                        }
                    }
                    TypedCol::Date(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) {
                                av_vec.push(AnyValue::Date(v));
                            } else {
                                av_vec.push(AnyValue::Null);
                            }
                        }
                    }
                    TypedCol::DateTime(vec, bitmap) => {
                        for (i, v) in vec.into_iter().enumerate() {
                            if bitmap.get(i) {
                                av_vec.push(AnyValue::Datetime(v, TimeUnit::Nanoseconds, None));
                            } else {
                                av_vec.push(AnyValue::Null);
                            }
                        }
                    }
                    TypedCol::Null(len) => av_vec.resize(len, AnyValue::Null),
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
            Data::DateTime(v) => {
                AnyValue::Datetime(v.to_timestamp_nanos(), TimeUnit::Nanoseconds, None)
            }
            Data::DateTimeIso(v) => AnyValue::StringOwned(v),
            Data::DurationIso(v) => AnyValue::StringOwned(v),
            Data::SharedStringRef(idx) => {
                AnyValue::StringOwned(PlSmallStr::from_string(idx.to_string()))
            }
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
        Data::String(_)
        | Data::SharedStringRef(_)
        | Data::DateTimeIso(_)
        | Data::DurationIso(_) => DataType::String,
        Data::DateTime(_) => DataType::Datetime(TimeUnit::Nanoseconds, None),
        Data::Error(_) | Data::Empty => DataType::Null,
    }
}

fn data_kind(data: &Data) -> &'static str {
    match data {
        Data::Int(_) => "Int64",
        Data::Float(_) => "Float64",
        Data::String(_) | Data::SharedStringRef(_) => "String",
        Data::Bool(_) => "Boolean",
        Data::DateTime(_) => "Datetime",
        Data::DateTimeIso(_) => "DatetimeIso",
        Data::DurationIso(_) => "DurationIso",
        Data::Error(_) => "ExcelError",
        Data::Empty => "Null",
    }
}

fn cell_reference(row: u32, col: u32) -> String {
    let mut n = col as usize + 1;
    let mut letters = Vec::with_capacity(3);
    while n > 0 {
        let remainder = (n - 1) % 26;
        letters.push((b'A' + remainder as u8) as char);
        n = (n - 1) / 26;
    }
    letters.reverse();
    format!("{}{}", letters.into_iter().collect::<String>(), row + 1)
}

fn checked_timestamp_in_unit(
    value: crate::excel_types::ExcelDateTime,
    unit: TimeUnit,
) -> Option<i64> {
    let timestamp_millis = value.try_to_timestamp_millis()?;
    match unit {
        TimeUnit::Nanoseconds => timestamp_millis.checked_mul(1_000_000),
        TimeUnit::Microseconds => timestamp_millis.checked_mul(1_000),
        TimeUnit::Milliseconds => Some(timestamp_millis),
    }
}

fn excel_datetime_iso(value: crate::excel_types::ExcelDateTime) -> String {
    let (year, month, day, hour, minute, second, millisecond) = value.to_ymd_hms_milli();
    if hour == 0 && minute == 0 && second == 0 && millisecond == 0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else if millisecond == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millisecond:03}")
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

    /// 根据固定 Schema 一次性建立“源列索引 → 输出列索引”映射并创建原生 builder。
    /// 映射建立后，逐单元格路径不再查询列名，也不再推断或升级类型。
    fn configure_strict_schema(
        &mut self,
        schema: &Schema,
        has_header: bool,
    ) -> anyhow::Result<Vec<usize>> {
        if schema.is_empty() {
            return Err(anyhow::anyhow!("严格读取的 Schema 不能为空"));
        }

        for (name, dtype) in schema.iter() {
            TypedCol::new_strict(dtype, 0).map_err(|e| {
                anyhow::anyhow!("严格读取列 '{}' 的类型 {:?} 不可用：{e}", name, dtype)
            })?;
        }

        let source_to_output = if has_header {
            let mut seen = HashSet::with_capacity(self.headers.len());
            let mut mapping = Vec::with_capacity(self.headers.len());
            let mut extra = Vec::new();

            for name in &self.headers {
                if !seen.insert(name.clone()) {
                    return Err(StrictReadError::schema_mismatch(
                        SchemaMismatch::DuplicateHeader { name: name.clone() },
                    ));
                }
                if let Some((output_idx, _, _)) = schema.get_full(name) {
                    mapping.push(output_idx);
                } else {
                    extra.push(name.clone());
                    // 保持映射长度，错误会在下面统一返回，数据路径不会使用该值。
                    mapping.push(usize::MAX);
                }
            }

            let missing: Vec<String> = schema
                .iter_names()
                .filter(|name| !seen.contains(name.as_str()))
                .map(|name| name.to_string())
                .collect();
            if !missing.is_empty() || !extra.is_empty() {
                return Err(StrictReadError::schema_mismatch(
                    SchemaMismatch::HeaderColumns { missing, extra },
                ));
            }
            mapping
        } else {
            if self.headers.len() != schema.len() {
                return Err(StrictReadError::schema_mismatch(
                    SchemaMismatch::HeaderlessColumnCount {
                        actual: self.headers.len(),
                        expected: schema.len(),
                    },
                ));
            }
            (0..schema.len()).collect()
        };

        self.headers = schema.iter_names().map(ToString::to_string).collect();
        self.col_dtypes = schema
            .iter_values()
            .map(|dtype| Some(dtype.clone()))
            .collect();
        self.cols = schema
            .iter_values()
            .map(|dtype| TypedCol::new_strict(dtype, self.batch_size))
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(source_to_output)
    }

    /// 固定 Schema 的逐单元格热路径。
    #[cfg(test)]
    fn push_cell_strict(
        &mut self,
        cell: Cell<Data>,
        batch_row: usize,
        source_to_output: &[usize],
        is_1904: bool,
    ) -> anyhow::Result<()> {
        self.push_cell_strict_with_numeric_adapter(cell, batch_row, source_to_output, is_1904, None)
    }

    fn push_cell_strict_with_numeric_adapter(
        &mut self,
        cell: Cell<Data>,
        batch_row: usize,
        source_to_output: &[usize],
        is_1904: bool,
        numeric_adapter: Option<&dyn StrictNumericReadAdapter>,
    ) -> anyhow::Result<()> {
        let (row, source_col) = cell.get_position();
        let source_col = source_col as usize;
        let output_col = source_to_output.get(source_col).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "严格读取遇到 Schema 之外的单元格 {}",
                cell_reference(row, source_col as u32)
            )
        })?;
        let dtype = self.col_dtypes[output_col]
            .as_ref()
            .expect("固定 Schema 的列类型必须预先初始化");
        let field_name = self.headers[output_col].as_str();
        let strings = self.strings.as_deref();
        let numeric_override = match (numeric_adapter, cell.get_value(), dtype) {
            (
                Some(adapter),
                Data::Int(_) | Data::Float(_),
                DataType::Float64 | DataType::Decimal(_, _),
            ) => {
                let raw = cell.raw_numeric_lexeme().map(Cow::Borrowed).or_else(|| {
                    if let Data::Int(value) = cell.get_value() {
                        Some(Cow::Owned(value.to_string()))
                    } else {
                        None
                    }
                });
                raw.map(|raw| {
                    adapter.read_numeric(StrictNumericReadCell {
                        field_name,
                        output_column: output_col,
                        target_type: dtype,
                        row,
                        column: source_col as u32,
                        raw_numeric_lexeme: &raw,
                    })
                })
                .transpose()?
                .flatten()
            }
            // 文本、日期、布尔和 Null 不进入数值适配器。
            _ => None,
        };
        if let Some(value) = numeric_override {
            match (dtype, value) {
                (
                    DataType::Decimal(precision, _),
                    StrictNumericReadValue::DecimalCoefficient(value),
                ) if value.unsigned_abs() < 10_u128.pow(*precision as u32) => {}
                (DataType::Float64, StrictNumericReadValue::Float64(value))
                    if value.is_finite() => {}
                _ => {
                    return Err(anyhow::anyhow!(
                        "数值适配器结果不满足字段 '{field_name}' 的固定类型或范围"
                    ));
                }
            }
        }
        let date_serial_not_integral = matches!(dtype, DataType::Date)
            && matches!(
                cell.get_value(),
                Data::Int(_) | Data::Float(_) | Data::DateTime(_)
            )
            && cell
                .raw_numeric_lexeme()
                .is_some_and(|raw| exact_decimal_coefficient(raw, 38, 0).is_none());
        let float64_lexeme_loss = matches!(dtype, DataType::Float64)
            && match cell.get_value() {
                Data::Int(value) => match numeric_override {
                    Some(StrictNumericReadValue::Float64(output)) => output == 0.0 && *value != 0,
                    _ => (*value as f64) as i128 != i128::from(*value),
                },
                Data::Float(value) => {
                    let output = match numeric_override {
                        Some(StrictNumericReadValue::Float64(output)) => output,
                        _ => *value,
                    };
                    !value.is_finite()
                        || !output.is_finite()
                        || cell.raw_numeric_lexeme().is_some_and(|raw| {
                            ((*value == 0.0 || output == 0.0) && numeric_lexeme_is_nonzero(raw))
                                || (numeric_override.is_none()
                                    && integer_lexeme_is_inexact_in_float(raw, output))
                        })
                }
                _ => false,
            };
        let exact_numeric_text = if matches!(dtype, DataType::String) {
            match cell.get_value() {
                Data::Float(value) => cell.raw_numeric_lexeme().and_then(|raw| {
                    (!value.is_finite()
                        || (*value == 0.0 && numeric_lexeme_is_nonzero(raw))
                        || integer_lexeme_is_inexact_in_float(raw, *value))
                    .then(|| raw.to_string())
                }),
                _ => None,
            }
        } else {
            None
        };
        let decimal_coefficient = if let DataType::Decimal(precision, scale) = dtype {
            if let Some(StrictNumericReadValue::DecimalCoefficient(value)) = numeric_override {
                Some(value)
            } else {
                let raw = match cell.get_value() {
                    Data::Int(value) => Some(
                        cell.raw_numeric_lexeme()
                            .map(Cow::Borrowed)
                            .unwrap_or_else(|| Cow::Owned(value.to_string())),
                    ),
                    Data::Float(_) => cell.raw_numeric_lexeme().map(Cow::Borrowed),
                    _ => None,
                };
                if matches!(cell.get_value(), Data::Int(_) | Data::Float(_)) {
                    Some(
                        raw.and_then(|raw| exact_decimal_coefficient(&raw, *precision, *scale))
                            .ok_or_else(|| {
                                StrictReadError::schema_mismatch(
                                    SchemaMismatch::DecimalLexemeLoss {
                                        cell: cell_reference(row, source_col as u32),
                                        column: field_name.to_string(),
                                        precision: *precision,
                                        scale: *scale,
                                    },
                                )
                            })?,
                    )
                } else {
                    None
                }
            }
        } else {
            None
        };
        let temporal_value = if matches!(dtype, DataType::Date | DataType::Datetime(_, None)) {
            match cell.get_value() {
                Data::DateTime(value) => Some(value.as_f64()),
                Data::Int(value) => Some(*value as f64),
                Data::Float(value) => Some(*value),
                _ => None,
            }
            .map(|value| crate::excel_types::ExcelDateTime::new(value, is_1904))
        } else {
            None
        };
        let data = cell.into_value();
        let type_mismatch = |observed: &str| {
            StrictReadError::schema_mismatch(SchemaMismatch::CellPhysicalType {
                cell: cell_reference(row, source_col as u32),
                column: field_name.to_string(),
                expected: format!("{dtype:?}"),
                actual: observed.to_string(),
            })
        };
        let temporal_mismatch = |reason: &str| {
            StrictReadError::schema_mismatch(SchemaMismatch::TemporalValueMismatch {
                cell: cell_reference(row, source_col as u32),
                column: field_name.to_string(),
                expected: format!("{dtype:?}"),
                reason: reason.to_string(),
            })
        };

        if let Data::Error(error) = &data {
            return Err(anyhow::Error::new(StrictReadError::new(
                StrictReadErrorKind::ExcelCellError {
                    cell: cell_reference(row, source_col as u32),
                    column: field_name.to_string(),
                    error: error.to_string(),
                },
            )));
        }

        if float64_lexeme_loss {
            return Err(StrictReadError::schema_mismatch(
                SchemaMismatch::Float64LexemeLoss {
                    cell: cell_reference(row, source_col as u32),
                    column: field_name.to_string(),
                },
            ));
        }
        if date_serial_not_integral {
            return Err(temporal_mismatch("date_serial_not_exact_integer"));
        }

        let col = &mut self.cols[output_col];
        let current_len = col.len();
        if current_len > batch_row {
            return Err(anyhow::anyhow!(
                "严格读取失败：单元格 {}（字段 '{}'）重复或乱序",
                cell_reference(row, source_col as u32),
                field_name
            ));
        }
        col.pad_to(batch_row);

        if matches!(data, Data::Empty) {
            col.push_null();
            return Ok(());
        }

        if let Data::SharedStringRef(idx) = &data {
            if !matches!(dtype, DataType::String) {
                return Err(type_mismatch("String"));
            }
            let strings = strings.ok_or_else(|| {
                anyhow::anyhow!("严格读取共享字符串失败：工作簿尚未初始化 sharedStrings")
            })?;
            if !col.push_shared_string_ref_checked(*idx, strings) {
                return Err(anyhow::anyhow!(
                    "严格读取失败：单元格 {}（字段 '{}'）引用了不存在的共享字符串索引 {}",
                    cell_reference(row, source_col as u32),
                    field_name,
                    idx
                ));
            }
            return Ok(());
        }

        match (col, data) {
            (TypedCol::Int64(values, validity), Data::Int(value)) => {
                values.push(value);
                validity.push(true);
            }
            (TypedCol::Float64(values, validity), Data::Float(value)) => {
                values.push(match numeric_override {
                    Some(StrictNumericReadValue::Float64(output)) => output,
                    _ => value,
                });
                validity.push(true);
            }
            (TypedCol::Float64(values, validity), Data::Int(value)) => {
                values.push(match numeric_override {
                    Some(StrictNumericReadValue::Float64(output)) => output,
                    _ => value as f64,
                });
                validity.push(true);
            }
            (TypedCol::Decimal(values, validity, _, _), Data::Int(_) | Data::Float(_)) => {
                values.push(decimal_coefficient.expect("数值词法在严格读取前已检查"));
                validity.push(true);
            }
            (TypedCol::Bool(values, validity), Data::Bool(value)) => {
                values.push(value);
                validity.push(true);
            }
            (TypedCol::String(values), Data::String(value))
            | (TypedCol::String(values), Data::DateTimeIso(value))
            | (TypedCol::String(values), Data::DurationIso(value)) => {
                values.push_value(&value);
            }
            (TypedCol::String(values), Data::Int(value)) => {
                values.push_value(&value.to_string());
            }
            (TypedCol::String(values), Data::Float(value)) => {
                if let Some(raw) = exact_numeric_text {
                    values.push_value(&raw);
                } else {
                    values.push_value(&value.to_string());
                }
            }
            (TypedCol::String(values), Data::Bool(value)) => {
                values.push_value(if value { "true" } else { "false" });
            }
            (TypedCol::String(values), Data::DateTime(value)) => {
                values.push_value(&excel_datetime_iso(value));
            }
            (
                TypedCol::Date(values, validity),
                Data::DateTime(_) | Data::Int(_) | Data::Float(_),
            ) => {
                let value = temporal_value.expect("数值与原生日期已建立时间值");
                let days = value.try_to_unix_date_days().ok_or_else(|| {
                    if value.as_f64().is_finite() && value.as_f64().fract() != 0.0 {
                        temporal_mismatch("date_has_nonzero_time")
                    } else {
                        temporal_mismatch("temporal_out_of_range")
                    }
                })?;
                values.push(days);
                validity.push(true);
            }
            (
                TypedCol::DateTime(values, validity),
                Data::DateTime(_) | Data::Int(_) | Data::Float(_),
            ) => {
                let DataType::Datetime(unit, None) = dtype else {
                    unreachable!("Datetime builder 必须对应无时区 Datetime")
                };
                let value = temporal_value.expect("数值与原生日期已建立时间值");
                let timestamp = checked_timestamp_in_unit(value, *unit)
                    .ok_or_else(|| temporal_mismatch("temporal_out_of_range"))?;
                values.push(timestamp);
                validity.push(true);
            }
            (_, actual) => return Err(type_mismatch(data_kind(&actual))),
        }

        Ok(())
    }

    fn finish_dataframe_strict(&mut self, row_count: usize) -> anyhow::Result<DataFrame> {
        let mut columns = Vec::with_capacity(self.cols.len());
        for idx in 0..self.cols.len() {
            let dtype = self.col_dtypes[idx]
                .as_ref()
                .expect("固定 Schema 的列类型必须预先初始化");
            let replacement = TypedCol::new_strict(dtype, self.batch_size)?;
            let mut col = std::mem::replace(&mut self.cols[idx], replacement);
            if !col.matches_exact_dtype(dtype) {
                return Err(anyhow::anyhow!(
                    "严格读取内部错误：字段 '{}' 的 builder 与类型 {:?} 不一致",
                    self.headers[idx],
                    dtype
                ));
            }
            col.pad_to(row_count);
            let series = col.into_series(self.headers[idx].as_str().into(), dtype)?;
            columns.push(series.into());
        }
        Ok(DataFrame::new_infer_height(columns)?)
    }

    pub fn into_dataframe(&mut self) -> PolarsResult<DataFrame> {
        let max_len = self.cols.iter().map(|c| c.len()).max().unwrap_or(0);

        let columns: Vec<Column> = std::mem::take(&mut self.cols)
            .into_iter()
            .enumerate()
            .map(|(i, mut col)| {
                let name = self.headers.get(i).map(|s| s.as_str()).unwrap_or("unknown");
                let series = if matches!(col, TypedCol::Empty) {
                    // 全空列（只有 header 没数据）：零写入的 Null series
                    Series::new_null(name.into(), max_len)
                } else {
                    col.pad_to(max_len);
                    let dtype = self
                        .col_dtypes
                        .get(i)
                        .and_then(|d| d.as_ref())
                        .unwrap_or(&DataType::Null);
                    col.into_series(name.into(), dtype)?
                };
                Ok::<_, polars::error::PolarsError>(series.into())
            })
            .collect::<Result<Vec<_>, _>>()?;

        DataFrame::new_infer_height(columns)
    }
}

/// 将单元格 Data 解析为 header 字符串，shared string 会查表解析为实际内容。
pub(crate) fn cell_value_to_header(data: Data, strings: Option<&Arc<SharedStrings>>) -> String {
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
pub struct DataFrameIter<const STRICT_SCHEMA: bool = false> {
    workbook: Arc<XlsxWorkbook>,
    reader: SheetReader,
    fast: bool,
    preserve_numeric_lexemes: bool,
    fast_config: Option<crate::sheet_fast::FastConfig>,
    cols: TypedCols,
    strict_schema: Option<SchemaRef>,
    source_to_output: Vec<usize>,
    is_1904: bool,
    cell_cache: Option<Cell<Data>>,
    has_header: bool,
    header_found: bool,
    failed: bool,
    len: usize,                      // 总批次数
    batch_start_row: Option<u32>,    // 当前批次的起始绝对行号
    current_row_count: usize,        // 当前批次已收集的行数（用于批次截断）
    last_processed_row: Option<u32>, // 上一个处理的绝对行号（检测行切换)
    current_sheet_name: Option<String>,
    current_sheet_idx: Option<usize>,
    skip_top_rows: u32,
    skip_rows_sorted: Vec<u32>, // 排序后的 skip 行列表（单调递增游标查询）
    skip_rows_idx: usize,       // skip_rows_sorted 的当前游标
    current_row_skipped: bool,  // 缓存当前行是否被跳过
    header_plan: Option<crate::header::BoundHeaderPlan>,
    numeric_adapter: Option<Arc<dyn StrictNumericReadAdapter>>,
    read_started: bool,
}

/// 固定 Schema 的流式读取器。
///
/// const 泛型让严格和推断两条逐单元格路径分别单态化，严格模式不会给原有
/// `DataFrameIter` 热路径增加运行时模式分派。
pub type StrictDataFrameIter = DataFrameIter<true>;

/// 固定输出列的原始数值上下文。只为数值物理单元格及 Decimal/Float64 目标调用。
/// 不实现 Debug/Serialize，避免通过日志或报告意外保留原始词法。
pub struct StrictNumericReadCell<'a> {
    pub field_name: &'a str,
    pub output_column: usize,
    pub target_type: &'a DataType,
    pub row: u32,
    pub column: u32,
    pub raw_numeric_lexeme: &'a str,
}

impl StrictNumericReadCell<'_> {
    /// 仅在错误诊断需要时生成 Excel A1 坐标，不为每个成功值分配字符串。
    pub fn cell_reference(&self) -> String {
        cell_reference(self.row, self.column)
    }
}

/// 调用方按明确策略提供的数值结果；不能改写 Schema、字段名、Null 或文本。
#[derive(Debug, Clone, Copy)]
pub enum StrictNumericReadValue {
    /// 已按目标 Decimal scale 缩放的系数，库会再次校验目标 precision。
    DecimalCoefficient(i128),
    Float64(f64),
}

/// 严格读取的显式数值转换钩子。None 沿用库原来的 Exact 语义。
/// 调用方负责策略授权和有界审计；错误类型保留给调用方分类，失败批次不会输出。
pub trait StrictNumericReadAdapter: Send + Sync {
    fn read_numeric(
        &self,
        cell: StrictNumericReadCell<'_>,
    ) -> anyhow::Result<Option<StrictNumericReadValue>>;
}

/// 固定 Schema 严格读取中可供调用方 downcast 的明确错误。
///
/// 只有结构或物理单元格类型不匹配，以及 Excel 自身的单元格错误会归入这些类别；
/// 其它解析错误仍保留为普通 `anyhow` 错误，不会被自动放行或误分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictReadError {
    /// 有工作表上下文时包含工作表名称。
    pub sheet: Option<String>,
    /// 严格读取错误类别。
    pub kind: StrictReadErrorKind,
}

/// 严格读取错误的可机读类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrictReadErrorKind {
    /// 输入结构与固定 Schema 不匹配。
    SchemaMismatch(SchemaMismatch),
    /// 单元格本身包含 Excel 错误值，独立于 SchemaMismatch。
    ExcelCellError {
        cell: String,
        column: String,
        error: String,
    },
}

/// 固定 Schema 可识别的结构和物理类型不匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaMismatch {
    /// 要求表头时，工作表没有可用单元格作为表头。
    MissingHeader { message: String },
    /// 表头缺少 Schema 字段或包含额外字段。
    HeaderColumns {
        missing: Vec<String>,
        extra: Vec<String>,
    },
    /// 表头字段名重复。
    DuplicateHeader { name: String },
    /// 无表头模式下，物理列数与 Schema 字段数不同。
    HeaderlessColumnCount { actual: usize, expected: usize },
    /// 单元格的物理类型无法写入固定 Schema 字段。
    CellPhysicalType {
        cell: String,
        column: String,
        expected: String,
        actual: String,
    },
    /// 原始数值词法转为 Float64 会产生未获声明的整数损失或溢出/下溢。
    Float64LexemeLoss { cell: String, column: String },
    /// 原始数值词法无法无损写入指定精度和小数位数的 Decimal。
    DecimalLexemeLoss {
        cell: String,
        column: String,
        precision: usize,
        scale: usize,
    },
    /// 原生日期/数值无法满足 Date 的无时间要求或目标时间戳的物理范围。
    TemporalValueMismatch {
        cell: String,
        column: String,
        expected: String,
        reason: String,
    },
    /// 多级表头的实际列范围以外出现了非空数据，不得静默丢弃。
    ColumnOutsideHeader {
        cell: String,
        physical_column: u32,
        header_column_count: usize,
    },
}

impl StrictReadError {
    fn new(kind: StrictReadErrorKind) -> Self {
        Self { sheet: None, kind }
    }

    fn schema_mismatch(mismatch: SchemaMismatch) -> anyhow::Error {
        anyhow::Error::new(Self::new(StrictReadErrorKind::SchemaMismatch(mismatch)))
    }
}

impl std::fmt::Display for StrictReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(sheet) = &self.sheet {
            write!(f, "工作表 '{sheet}'：")?;
        }
        match &self.kind {
            StrictReadErrorKind::SchemaMismatch(mismatch) => match mismatch {
                SchemaMismatch::MissingHeader { message } => f.write_str(message),
                SchemaMismatch::HeaderColumns { missing, extra } => write!(
                    f,
                    "严格 Schema 与工作表表头不一致：缺少列 {:?}，多出列 {:?}",
                    missing, extra
                ),
                SchemaMismatch::DuplicateHeader { name } => {
                    write!(f, "严格读取要求表头唯一，但工作表中列 '{}' 重复", name)
                }
                SchemaMismatch::HeaderlessColumnCount { actual, expected } => write!(
                    f,
                    "无表头严格读取要求按位置一一对应，但工作表有 {} 列，Schema 有 {} 列",
                    actual, expected
                ),
                SchemaMismatch::CellPhysicalType {
                    cell,
                    column,
                    expected,
                    actual,
                } => write!(
                    f,
                    "严格读取失败：单元格 {}（字段 '{}'）期望 {}，实际为 {}",
                    cell, column, expected, actual
                ),
                SchemaMismatch::Float64LexemeLoss { cell, column } => write!(
                    f,
                    "严格读取失败：单元格 {}（字段 '{}'）转为 Float64 会丢失原始数值精度",
                    cell, column
                ),
                SchemaMismatch::DecimalLexemeLoss {
                    cell,
                    column,
                    precision,
                    scale,
                } => write!(
                    f,
                    "严格读取失败：单元格 {}（字段 '{}'）无法无损写入 Decimal({}, {})",
                    cell, column, precision, scale
                ),
                SchemaMismatch::TemporalValueMismatch {
                    cell,
                    column,
                    expected,
                    reason,
                } => write!(
                    f,
                    "严格读取失败：单元格 {}（字段 '{}'）无法写入 {}，原因 {}",
                    cell, column, expected, reason
                ),
                SchemaMismatch::ColumnOutsideHeader {
                    cell,
                    physical_column,
                    header_column_count,
                } => write!(
                    f,
                    "严格读取失败：单元格 {cell} 位于物理列 {physical_column}，超过表头定义的 {header_column_count} 列"
                ),
            },
            StrictReadErrorKind::ExcelCellError {
                cell,
                column,
                error,
            } => write!(
                f,
                "严格读取失败：单元格 {}（字段 '{}'）包含 Excel 错误 {}",
                cell, column, error
            ),
        }
    }
}

impl std::error::Error for StrictReadError {}

fn sheet_context_label(sheet_name: Option<&str>, sheet_idx: Option<usize>) -> String {
    sheet_name
        .map(str::to_string)
        .or_else(|| sheet_idx.map(|idx| format!("#{idx}")))
        .unwrap_or_else(|| "#0".to_string())
}

fn add_strict_sheet_context(error: anyhow::Error, sheet: &str) -> anyhow::Error {
    match error.downcast::<StrictReadError>() {
        Ok(mut error) => {
            error.sheet = Some(sheet.to_string());
            anyhow::Error::new(error)
        }
        Err(error) => error,
    }
}

/// 严格 XLSX 读取选项。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StrictReadOptions {
    /// 在查找表头或首条数据前，跳过 Excel 工作表顶部的物理行数。
    ///
    /// 该值是行数，不是 `skip_rows` 使用的工作表行号列表；默认不跳过。
    pub skip_top_rows: u32,
}

/// 全 Sheet 扫描中实际出现过的非空单元格物理类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SheetPhysicalKind {
    Int64,
    Float64,
    Boolean,
    Text,
    DateTime,
    DateTimeIso,
    DurationIso,
}

/// 一列的完整观测结果；不包含任何原始单元格值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetFieldInspection {
    pub name: String,
    pub inferred_type: DataType,
    pub physical_kinds: BTreeSet<SheetPhysicalKind>,
    pub non_null_count: u64,
    pub null_count: u64,
    /// 存在不能无损转成 Float64 的原始整数词法，或混合列中的 Int64 无法无损提升。
    pub requires_exact_numeric_review: bool,
    /// 需要精确数值时，全列原始词法无法统一表示为 precision <= 38 的 Decimal。
    /// 此时 inferred_type 仅供诊断，调用方不得据此物化可复用结果。
    pub unsupported_exact_numeric: bool,
}

/// 按现有表头和空行语义完整扫描 Sheet 得到的 Schema 证据。
/// 行数仅统计至少含一个单元格记录且未被跳过的数据行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetSchemaInspection {
    pub sheet: String,
    pub row_count: u64,
    pub fields: Vec<SheetFieldInspection>,
}

#[derive(Debug)]
struct FieldScanAccumulator {
    name: String,
    inferred_type: Option<DataType>,
    physical_kinds: BTreeSet<SheetPhysicalKind>,
    non_null_count: u64,
    last_cell_row: Option<u32>,
    saw_inexact_int_for_float: bool,
    saw_unrepresentable_numeric_lexeme: bool,
    max_numeric_integer_digits: usize,
    max_numeric_scale: usize,
    numeric_decimal_width_supported: bool,
}

impl FieldScanAccumulator {
    fn new(name: String) -> Self {
        Self {
            name,
            inferred_type: None,
            physical_kinds: BTreeSet::new(),
            non_null_count: 0,
            last_cell_row: None,
            saw_inexact_int_for_float: false,
            saw_unrepresentable_numeric_lexeme: false,
            max_numeric_integer_digits: 0,
            max_numeric_scale: 0,
            numeric_decimal_width_supported: true,
        }
    }

    fn finish(self, row_count: u64) -> SheetFieldInspection {
        let requires_exact_numeric_review = self.saw_unrepresentable_numeric_lexeme
            || (self.saw_inexact_int_for_float
                && self.physical_kinds.contains(&SheetPhysicalKind::Float64));
        let numeric_only = self
            .physical_kinds
            .iter()
            .all(|kind| matches!(kind, SheetPhysicalKind::Int64 | SheetPhysicalKind::Float64));
        let precision = self
            .max_numeric_integer_digits
            .checked_add(self.max_numeric_scale)
            .map(|value| value.max(1));
        let decimal_supported =
            self.numeric_decimal_width_supported && precision.is_some_and(|value| value <= 38);
        let unsupported_exact_numeric =
            requires_exact_numeric_review && numeric_only && !decimal_supported;
        let inferred_type = if requires_exact_numeric_review && numeric_only && decimal_supported {
            DataType::Decimal(
                precision.expect("Decimal 精度已检查"),
                self.max_numeric_scale,
            )
        } else {
            self.inferred_type.unwrap_or(DataType::Null)
        };
        SheetFieldInspection {
            name: self.name,
            inferred_type,
            physical_kinds: self.physical_kinds,
            non_null_count: self.non_null_count,
            null_count: row_count - self.non_null_count,
            requires_exact_numeric_review,
            unsupported_exact_numeric,
        }
    }

    fn observe_numeric_lexeme(&mut self, raw: Option<&str>, data: &Data) {
        if !matches!(data, Data::Int(_) | Data::Float(_)) {
            return;
        }
        let Some(raw) = raw else {
            self.numeric_decimal_width_supported = false;
            return;
        };
        match numeric_decimal_width(raw) {
            Some((integer_digits, scale)) => {
                self.max_numeric_integer_digits =
                    self.max_numeric_integer_digits.max(integer_digits);
                self.max_numeric_scale = self.max_numeric_scale.max(scale);
            }
            None => self.numeric_decimal_width_supported = false,
        }
        if let Data::Float(value) = data {
            self.saw_unrepresentable_numeric_lexeme |= !value.is_finite()
                || (*value == 0.0 && numeric_lexeme_is_nonzero(raw))
                || integer_lexeme_is_inexact_in_float(raw, *value);
        }
    }
}

fn numeric_lexeme_is_nonzero(raw: &str) -> bool {
    raw.split_once(['e', 'E'])
        .map(|(mantissa, _)| mantissa)
        .unwrap_or(raw)
        .bytes()
        .any(|byte| matches!(byte, b'1'..=b'9'))
}

/// 按目标 Decimal 的 scale 直接解析 XLSX `<v>`，不经过 f64，也不进行隐式舍入。
/// 前后无意义的零不占精度；超出目标 scale 的非零数字则严格失败。
fn exact_decimal_coefficient(raw: &str, precision: usize, scale: usize) -> Option<i128> {
    if !(1..=38).contains(&precision) || scale > precision {
        return None;
    }
    let (negative, unsigned) = match raw.as_bytes().first() {
        Some(b'-') => (true, &raw[1..]),
        Some(b'+') => (false, &raw[1..]),
        _ => (false, raw),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().ok()?,
        ),
        None => (unsigned, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let total_digits = integer.len().checked_add(fraction.len())?;
    if total_digits == 0 {
        return None;
    }
    let digits = integer.bytes().chain(fraction.bytes());
    if !digits.clone().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let first_nonzero = digits.clone().position(|byte| byte != b'0');
    let Some(first_nonzero) = first_nonzero else {
        return Some(0);
    };
    let last_nonzero = digits
        .clone()
        .enumerate()
        .filter_map(|(index, byte)| (byte != b'0').then_some(index))
        .last()?;
    let trailing_zeros = total_digits.checked_sub(last_nonzero)?.checked_sub(1)?;
    let core_digits = last_nonzero.checked_sub(first_nonzero)?.checked_add(1)?;
    let shift =
        i128::from(exponent) - fraction.len() as i128 + scale as i128 + trailing_zeros as i128;
    let shift = usize::try_from(shift).ok()?;
    if core_digits.checked_add(shift)? > precision {
        return None;
    }
    let mut coefficient = 0_i128;
    for byte in digits.skip(first_nonzero).take(core_digits) {
        coefficient = coefficient
            .checked_mul(10)?
            .checked_add(i128::from(byte - b'0'))?;
    }
    for _ in 0..shift {
        coefficient = coefficient.checked_mul(10)?;
    }
    if negative {
        coefficient.checked_neg()
    } else {
        Some(coefficient)
    }
}

/// 根据 `<v>` 的十进制词法计算无损 Decimal 所需的整数位数与 scale。
/// 返回 None 表示超过 Decimal128 的宽度，或词法不符合有限十进制数。
fn numeric_decimal_width(raw: &str) -> Option<(usize, usize)> {
    let unsigned = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().ok()?,
        ),
        None => (unsigned, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if integer.is_empty() && fraction.is_empty() {
        return None;
    }
    if !integer
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = integer.as_bytes().iter().chain(fraction.as_bytes().iter());
    let first_nonzero = digits.clone().position(|byte| *byte != b'0');
    let Some(first_nonzero) = first_nonzero else {
        return Some((0, 0));
    };
    let last_nonzero = digits
        .enumerate()
        .filter_map(|(index, byte)| (*byte != b'0').then_some(index))
        .last()?;
    let total_digits = integer.len().checked_add(fraction.len())?;
    let significant_digits = last_nonzero.checked_sub(first_nonzero)?.checked_add(1)?;
    let trailing_zeros = total_digits.checked_sub(last_nonzero)?.checked_sub(1)?;
    let shift = exponent
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing_zeros).ok()?)?;
    if shift >= 0 {
        let integer_digits = significant_digits.checked_add(usize::try_from(shift).ok()?)?;
        (integer_digits <= 38).then_some((integer_digits, 0))
    } else {
        let scale = usize::try_from(shift.checked_neg()?).ok()?;
        let integer_digits = significant_digits.saturating_sub(scale);
        integer_digits
            .checked_add(scale)
            .filter(|precision| *precision <= 38)
            .map(|_| (integer_digits, scale))
    }
}

fn integer_lexeme_is_inexact_in_float(raw: &str, value: f64) -> bool {
    let unsigned = raw.strip_prefix(['+', '-']).unwrap_or(raw);
    if unsigned.is_empty() || !unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(integer) = raw.parse::<i128>() else {
        return true;
    };
    !value.is_finite()
        || value.fract() != 0.0
        || value >= i128::MAX as f64
        || value < i128::MIN as f64
        || value as i128 != integer
}

fn sheet_physical_kind(data: &Data) -> Option<SheetPhysicalKind> {
    match data {
        Data::Int(_) => Some(SheetPhysicalKind::Int64),
        Data::Float(_) => Some(SheetPhysicalKind::Float64),
        Data::Bool(_) => Some(SheetPhysicalKind::Boolean),
        Data::String(_) | Data::SharedStringRef(_) => Some(SheetPhysicalKind::Text),
        Data::DateTime(_) => Some(SheetPhysicalKind::DateTime),
        Data::DateTimeIso(_) => Some(SheetPhysicalKind::DateTimeIso),
        Data::DurationIso(_) => Some(SheetPhysicalKind::DurationIso),
        Data::Empty | Data::Error(_) => None,
    }
}

fn merge_sheet_dtype(current: Option<DataType>, observed: DataType) -> DataType {
    match (current, observed) {
        (None, data_type) => data_type,
        (Some(DataType::Int64), DataType::Float64) | (Some(DataType::Float64), DataType::Int64) => {
            DataType::Float64
        }
        (Some(previous), data_type) if previous == data_type => previous,
        _ => DataType::String,
    }
}

/// 不生成 DataFrame，完整扫描一个已打开工作簿的选定 Sheet。
/// 仅保存每列的类型集合、Null/非 Null 计数和精度风险；不同端口可复用同一 Workbook。
///
/// # Errors
/// Sheet 缺失、表头重复、单元格错误或文件结构损坏时返回错误。
#[allow(clippy::too_many_arguments)]
pub fn inspect_sheet_schema_from_workbook(
    workbook: Arc<XlsxWorkbook>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
    options: StrictReadOptions,
) -> anyhow::Result<SheetSchemaInspection> {
    DataFrameIter::<false>::from_workbook_inner(
        Some(1),
        workbook,
        sheet_name,
        sheet_idx,
        has_header,
        skip_rows,
        options,
        false,
        None,
        None,
        true,
        None,
    )?
    .scan_full_schema()
}

/// 使用同一表头证据完整扫描数据类型，不缓存 DataFrame，也不采用 AcceptedSchema 类型。
/// 字段名称仅来自调用方显式绑定的实际标题路径。
pub fn inspect_sheet_schema_with_header_plan(
    workbook: Arc<XlsxWorkbook>,
    plan: &crate::header::BoundHeaderPlan,
    skip_rows: Option<&[u32]>,
) -> anyhow::Result<SheetSchemaInspection> {
    plan.validate_workbook(&workbook)?;
    DataFrameIter::<false>::from_workbook_inner(
        Some(1),
        workbook,
        Some(plan.header_plan().sheet_name()),
        None,
        true,
        skip_rows,
        StrictReadOptions {
            skip_top_rows: plan.header_plan().options().skip_top_rows,
        },
        false,
        None,
        None,
        true,
        Some(plan),
    )?
    .scan_full_schema()
}

impl DataFrameIter<false> {
    fn scan_full_schema(mut self) -> anyhow::Result<SheetSchemaInspection> {
        let sheet = sheet_context_label(self.current_sheet_name.as_deref(), self.current_sheet_idx);
        if self.has_header && !self.header_found {
            return Err(add_strict_sheet_context(
                StrictReadError::schema_mismatch(SchemaMismatch::MissingHeader {
                    message: "全 Sheet 扫描缺少表头：工作表没有任何单元格".to_string(),
                }),
                &sheet,
            ));
        }
        let mut names = HashSet::new();
        let mut fields = Vec::with_capacity(self.cols.headers.len());
        for name in std::mem::take(&mut self.cols.headers) {
            if !names.insert(name.clone()) {
                return Err(add_strict_sheet_context(
                    StrictReadError::schema_mismatch(SchemaMismatch::DuplicateHeader { name }),
                    &sheet,
                ));
            }
            fields.push(FieldScanAccumulator::new(name));
        }

        let mut row_count = 0_u64;
        let mut current_data_row = None;
        let mut last_physical_row = None;
        loop {
            let cell = match self.cell_cache.take() {
                Some(cell) => cell,
                None => match self.reader.next_cell() {
                    Ok(Some(cell)) => cell,
                    Ok(None) => break,
                    Err(error) => return Err(anyhow::anyhow!("工作表 '{sheet}'：{error}")),
                },
            };
            let (row, column) = cell.get_position();
            if last_physical_row.is_some_and(|previous| row < previous) {
                return Err(anyhow::anyhow!("工作表 '{sheet}'：单元格行号未按顺序出现"));
            }
            last_physical_row = Some(row);
            if self.is_row_skipped(row) {
                continue;
            }
            if current_data_row != Some(row) {
                row_count = row_count
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("工作表 '{sheet}'：数据行数溢出"))?;
                current_data_row = Some(row);
            }
            let column = column as usize;
            if self.header_plan.is_some()
                && column >= fields.len()
                && matches!(cell.get_value(), Data::Empty)
            {
                continue;
            }
            while fields.len() <= column {
                let name = if self.has_header {
                    format!("Unknown_{}", fields.len())
                } else {
                    format!("col_{}", fields.len())
                };
                if !names.insert(name.clone()) {
                    return Err(add_strict_sheet_context(
                        StrictReadError::schema_mismatch(SchemaMismatch::DuplicateHeader { name }),
                        &sheet,
                    ));
                }
                fields.push(FieldScanAccumulator::new(name));
            }
            let field = &mut fields[column];
            if field.last_cell_row == Some(row) {
                return Err(anyhow::anyhow!(
                    "工作表 '{sheet}'：单元格 {} 重复",
                    cell_reference(row, column as u32)
                ));
            }
            field.last_cell_row = Some(row);
            field.observe_numeric_lexeme(cell.raw_numeric_lexeme(), cell.get_value());
            let data = cell.into_value();
            if let Data::Error(error) = &data {
                return Err(add_strict_sheet_context(
                    anyhow::Error::new(StrictReadError::new(StrictReadErrorKind::ExcelCellError {
                        cell: cell_reference(row, column as u32),
                        column: field.name.clone(),
                        error: error.to_string(),
                    })),
                    &sheet,
                ));
            }
            if matches!(data, Data::Empty) {
                continue;
            }
            if let Data::SharedStringRef(index) = &data
                && self.reader.strings().offsets.get(*index).is_none()
            {
                return Err(anyhow::anyhow!(
                    "工作表 '{sheet}'：单元格 {} 引用了不存在的 shared string",
                    cell_reference(row, column as u32)
                ));
            }
            let kind = sheet_physical_kind(&data).expect("非空、非错误单元格应有物理类型");
            field.physical_kinds.insert(kind);
            field.inferred_type = Some(merge_sheet_dtype(
                field.inferred_type.take(),
                data_to_dtype(&data),
            ));
            field.non_null_count = field
                .non_null_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("工作表 '{sheet}'：非空单元格数溢出"))?;
            if let Data::Int(value) = &data {
                field.saw_inexact_int_for_float |= (*value as f64) as i128 != i128::from(*value);
            }
        }

        Ok(SheetSchemaInspection {
            sheet,
            row_count,
            fields: fields
                .into_iter()
                .map(|field| field.finish(row_count))
                .collect(),
        })
    }

    pub fn new<P>(
        batch_size: Option<usize>,
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        let workbook = if fast {
            Arc::new(XlsxWorkbook::open_fast(path)?)
        } else {
            Arc::new(XlsxWorkbook::open(path)?)
        };
        Self::from_workbook(
            batch_size, workbook, sheet_name, sheet_idx, has_header, skip_rows, fast, config,
        )
    }

    pub fn from_workbook(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self> {
        Self::from_workbook_inner(
            batch_size,
            workbook,
            sheet_name,
            sheet_idx,
            has_header,
            skip_rows,
            StrictReadOptions::default(),
            fast,
            config,
            None,
            false,
            None,
        )
    }
}

impl DataFrameIter<true> {
    /// 在首个批次读取前固定数值转换策略；不影响原有构造器的 Exact 默认行为。
    pub fn with_numeric_read_adapter(
        mut self,
        adapter: Arc<dyn StrictNumericReadAdapter>,
    ) -> anyhow::Result<Self> {
        if self.read_started {
            return Err(anyhow::anyhow!("数值读取适配器必须在首个批次读取前设置"));
        }
        self.numeric_adapter = Some(adapter);
        Ok(self)
    }
    /// 使用固定 Polars Schema 打开工作簿。
    ///
    /// 表头字段集合必须与 Schema 完全一致，但物理列顺序可以不同；输出始终按
    /// Schema 顺序排列。类型不匹配会在对应 batch 返回错误，且迭代器随即熔断。
    pub fn new_with_schema<P>(
        batch_size: Option<usize>,
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        schema: SchemaRef,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        Self::new_with_schema_and_options(
            batch_size,
            path,
            sheet_name,
            sheet_idx,
            has_header,
            skip_rows,
            schema,
            StrictReadOptions::default(),
            fast,
            config,
        )
    }

    /// 使用固定 Schema 和额外读取选项打开工作簿。
    pub fn new_with_schema_and_options<P>(
        batch_size: Option<usize>,
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        schema: SchemaRef,
        options: StrictReadOptions,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self>
    where
        P: AsRef<Path>,
    {
        let workbook = if fast {
            Arc::new(XlsxWorkbook::open_fast(path)?)
        } else {
            Arc::new(XlsxWorkbook::open(path)?)
        };
        Self::from_workbook_with_schema_and_options(
            batch_size, workbook, sheet_name, sheet_idx, has_header, skip_rows, schema, options,
            fast, config,
        )
    }

    /// 从已打开的工作簿创建固定 Schema 读取器。
    pub fn from_workbook_with_schema(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        schema: SchemaRef,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self> {
        Self::from_workbook_with_schema_and_options(
            batch_size,
            workbook,
            sheet_name,
            sheet_idx,
            has_header,
            skip_rows,
            schema,
            StrictReadOptions::default(),
            fast,
            config,
        )
    }

    /// 从已打开的工作簿创建带额外读取选项的固定 Schema 读取器。
    pub fn from_workbook_with_schema_and_options(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        schema: SchemaRef,
        options: StrictReadOptions,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self> {
        Self::from_workbook_inner(
            batch_size,
            workbook,
            sheet_name,
            sheet_idx,
            has_header,
            skip_rows,
            options,
            fast,
            config,
            Some(schema),
            true,
            None,
        )
    }

    /// 按库生成的多级表头计划严格读取。字段路径和输出名称已显式绑定，
    /// 不在这里拆分字符串、传播合并值或推断类型。
    #[allow(clippy::too_many_arguments)]
    pub fn from_workbook_with_header_plan(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        plan: &crate::header::BoundHeaderPlan,
        schema: SchemaRef,
        skip_rows: Option<&[u32]>,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
    ) -> anyhow::Result<Self> {
        plan.validate_workbook(&workbook)?;
        Self::from_workbook_inner(
            batch_size,
            workbook,
            Some(plan.header_plan().sheet_name()),
            None,
            true,
            skip_rows,
            StrictReadOptions {
                skip_top_rows: plan.header_plan().options().skip_top_rows,
            },
            fast,
            config,
            Some(schema),
            true,
            Some(plan),
        )
    }
}

impl<const STRICT_SCHEMA: bool> DataFrameIter<STRICT_SCHEMA> {
    #[allow(clippy::too_many_arguments)]
    fn from_workbook_inner(
        batch_size: Option<usize>,
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        has_header: bool,
        skip_rows: Option<&[u32]>,
        options: StrictReadOptions,
        fast: bool,
        config: Option<crate::sheet_fast::FastConfig>,
        strict_schema: Option<SchemaRef>,
        preserve_numeric_lexemes: bool,
        header_plan: Option<&crate::header::BoundHeaderPlan>,
    ) -> anyhow::Result<Self> {
        if STRICT_SCHEMA != strict_schema.is_some() {
            return Err(anyhow::anyhow!("严格读取器与 Schema 配置不一致"));
        }
        if let Some(schema) = strict_schema.as_deref() {
            if schema.is_empty() {
                return Err(anyhow::anyhow!("严格读取的 Schema 不能为空"));
            }
            for (name, dtype) in schema.iter() {
                TypedCol::new_strict(dtype, 0).map_err(|e| {
                    anyhow::anyhow!("严格读取列 '{}' 的类型 {:?} 不可用：{e}", name, dtype)
                })?;
            }
        }

        let fast_config = fast.then(|| config.unwrap_or_default());
        let cfg_ref = fast_config.as_ref();
        let reader = if fast {
            SheetReader::Fast(SheetFastReader::from_workbook_with_numeric_lexemes(
                Arc::clone(&workbook),
                sheet_name,
                sheet_idx,
                cfg_ref,
                preserve_numeric_lexemes,
            )?)
        } else {
            SheetReader::Stream(
                XlsxStreamReader::from_workbook(Arc::clone(&workbook), sheet_name, sheet_idx)?
                    .with_numeric_lexemes(preserve_numeric_lexemes),
            )
        };
        let dim = reader.dimensions();
        let batch_size = match batch_size {
            Some(0) => return Err(anyhow::anyhow!("batch_size 必须大于 0")),
            Some(s) => s,
            None => dim.end.0 as usize + if has_header { 0 } else { 1 },
        }
        .max(1);
        let column_dim = header_plan.map_or(dim, |plan| Dimensions {
            start: (0, 0),
            end: (dim.end.0, plan.header_plan().columns().len() as u32 - 1),
        });
        let mut cols = TypedCols::new(&column_dim, batch_size);
        cols.strings = Some(Arc::clone(reader.strings()));
        let mut skip_rows_sorted: Vec<u32> = skip_rows
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        skip_rows_sorted.sort_unstable();
        let mut iter = Self {
            is_1904: workbook.is_1904(),
            workbook,
            reader,
            fast_config,
            preserve_numeric_lexemes,
            cols,
            strict_schema,
            source_to_output: Vec::new(),
            cell_cache: None,
            has_header,
            header_found: false,
            fast,
            failed: false,
            len: 0,
            batch_start_row: None,
            current_row_count: 0,
            last_processed_row: None,
            current_sheet_name: sheet_name.map(|s| s.to_string()),
            current_sheet_idx: sheet_idx,
            skip_top_rows: options.skip_top_rows,
            skip_rows_sorted,
            skip_rows_idx: 0,
            current_row_skipped: false,
            header_plan: header_plan.cloned(),
            numeric_adapter: None,
            read_started: false,
        };
        let sheet_label = sheet_context_label(sheet_name, sheet_idx);
        if let Err(error) = iter.find_header(batch_size) {
            return Err(add_strict_sheet_context(error, &sheet_label));
        }
        if STRICT_SCHEMA {
            let schema = Arc::clone(
                iter.strict_schema
                    .as_ref()
                    .expect("严格读取器必须持有 Schema"),
            );
            let result = iter
                .cols
                .configure_strict_schema(schema.as_ref(), has_header);
            iter.source_to_output =
                result.map_err(|error| add_strict_sheet_context(error, &sheet_label))?;
        }

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
        if self.header_plan.is_some() {
            return Err(crate::header::HeaderPlanError::CannotSelectSheetWithPlan.into());
        }
        if self.numeric_adapter.is_some() {
            return Err(anyhow::anyhow!(
                "数值适配器已固定当前 Sheet；切换 Sheet 须创建新读取器"
            ));
        }
        self.reader = if self.fast {
            SheetReader::Fast(SheetFastReader::from_workbook_with_numeric_lexemes(
                Arc::clone(&self.workbook),
                sheet_name,
                sheet_idx,
                self.fast_config.as_ref(),
                self.preserve_numeric_lexemes,
            )?)
        } else {
            SheetReader::Stream(
                XlsxStreamReader::from_workbook(Arc::clone(&self.workbook), sheet_name, sheet_idx)?
                    .with_numeric_lexemes(self.preserve_numeric_lexemes),
            )
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
        self.failed = false;
        self.read_started = false;
        self.source_to_output.clear();
        self.skip_rows_idx = 0;
        self.current_row_skipped = false;
        self.header_found = false;
        let sheet_label = sheet_context_label(sheet_name, sheet_idx);
        if let Err(error) = self.find_header(batch_size) {
            return Err(add_strict_sheet_context(error, &sheet_label));
        }
        if STRICT_SCHEMA {
            let schema = Arc::clone(
                self.strict_schema
                    .as_ref()
                    .expect("严格读取器必须持有 Schema"),
            );
            let result = self
                .cols
                .configure_strict_schema(schema.as_ref(), self.has_header);
            self.source_to_output =
                result.map_err(|error| add_strict_sheet_context(error, &sheet_label))?;
        }
        Ok(())
    }

    fn find_header(&mut self, batch_size: usize) -> anyhow::Result<()> {
        if let Some(plan) = &self.header_plan {
            let data_start = plan.header_plan().data_start_row();
            self.cols.headers = plan.field_names().to_vec();
            self.header_found = true;
            self.cell_cache = loop {
                match self.reader.next_cell()? {
                    Some(cell) if cell.get_position().0 < data_start => continue,
                    next => break next,
                }
            };
            self.len = if self.cell_cache.is_some() {
                (self.reader.dimensions().end.0.saturating_sub(data_start) as usize + 1)
                    .div_ceil(batch_size)
            } else {
                0
            };
            return Ok(());
        }
        let strings = self.cols.strings.clone();
        let first_cell = loop {
            match self.reader.next_cell()? {
                Some(cell) if cell.get_position().0 < self.skip_top_rows => continue,
                next => break next,
            }
        };
        let first_cell = match first_cell {
            Some(cell) => cell,
            None => {
                if STRICT_SCHEMA && self.has_header {
                    let message = if self.skip_top_rows > 0 {
                        "严格读取缺少表头：跳过顶部物理行后工作表没有剩余单元格"
                    } else {
                        "严格读取缺少表头：工作表没有任何单元格"
                    };
                    return Err(StrictReadError::schema_mismatch(
                        SchemaMismatch::MissingHeader {
                            message: message.to_string(),
                        },
                    ));
                }
                if STRICT_SCHEMA && self.skip_top_rows > 0 {
                    if let Some(schema) = &self.strict_schema {
                        self.cols.headers = schema.iter_names().map(ToString::to_string).collect();
                    }
                }
                if self.cols.headers.is_empty() {
                    self.cols
                        .cols
                        .iter()
                        .enumerate()
                        .for_each(|(i, _)| self.cols.headers.push(format!("col_{}", i)));
                }
                self.len = 0;
                return Ok(());
            }
        };
        let (first_x, first_y) = first_cell.get_position();
        let mut total_rows: usize;
        if self.has_header {
            self.header_found = true;
            while self.cols.headers.len() < first_y as usize {
                self.cols
                    .headers
                    .push(format!("Unknown_{}", self.cols.headers.len()));
            }
            self.cols.headers.push(cell_value_to_header(
                first_cell.into_value(),
                strings.as_ref(),
            ));
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
                            let mut value =
                                cell_value_to_header(cell.into_value(), strings.as_ref());
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
        let skip_count = self
            .skip_rows_sorted
            .iter()
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
        let result = if STRICT_SCHEMA {
            self.cols.finish_dataframe_strict(self.current_row_count)
        } else {
            self.cols
                .into_dataframe()
                .map_err(|error| anyhow::anyhow!("{error}"))
        };
        let df = match result {
            Ok(df) => df,
            Err(error) => return self.stop_with_error(error),
        };
        self.batch_start_row = None;
        self.current_row_count = 0;
        self.last_processed_row = None;
        Some(Ok(df))
    }

    #[inline(always)]
    fn push_data_cell(&mut self, cell: Cell<Data>, batch_row: usize) -> anyhow::Result<()> {
        if STRICT_SCHEMA {
            if let Some(plan) = &self.header_plan {
                let (row, column) = cell.get_position();
                if column as usize >= plan.header_plan().columns().len() {
                    // 范围外仅有格式的空单元格不扩大数据列；真实数据必须报结构漂移。
                    if matches!(cell.get_value(), Data::Empty) {
                        return Ok(());
                    }
                    return Err(StrictReadError::schema_mismatch(
                        SchemaMismatch::ColumnOutsideHeader {
                            cell: cell_reference(row, column),
                            physical_column: column,
                            header_column_count: plan.header_plan().columns().len(),
                        },
                    ));
                }
            }
            self.cols.push_cell_strict_with_numeric_adapter(
                cell,
                batch_row,
                &self.source_to_output,
                self.is_1904,
                self.numeric_adapter.as_deref(),
            )
        } else {
            self.cols.push_cell(cell, batch_row)
        }
    }

    fn stop_with_error(&mut self, error: anyhow::Error) -> Option<anyhow::Result<DataFrame>> {
        self.failed = true;
        self.len = 0;
        let sheet = sheet_context_label(self.current_sheet_name.as_deref(), self.current_sheet_idx);
        let error = add_strict_sheet_context(error, &sheet);
        if error.downcast_ref::<StrictReadError>().is_some() {
            Some(Err(error))
        } else {
            // 保留既有 Display 的完整信息，同时保留回调错误类型供 ETL downcast。
            let message = format!("工作表 '{sheet}'：{error}");
            Some(Err(error.context(message)))
        }
    }
}

impl<const STRICT_SCHEMA: bool> Iterator for DataFrameIter<STRICT_SCHEMA> {
    type Item = anyhow::Result<DataFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        self.read_started = true;
        if self.failed {
            return None;
        }

        // 获取第一个非跳过的 cell 作为 batch 起点
        if self.batch_start_row.is_none() {
            loop {
                let cell = if let Some(c) = self.cell_cache.take() {
                    c
                } else {
                    match self.reader.next_cell() {
                        Ok(Some(c)) => c,
                        Ok(None) => {
                            self.len = 0;
                            return None;
                        }
                        Err(error) => return self.stop_with_error(error),
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
                if let Err(error) = self.push_data_cell(cell, 0) {
                    return self.stop_with_error(error);
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
                    if let Err(error) = self.push_data_cell(cell, batch_row) {
                        return self.stop_with_error(error);
                    }
                }
                Ok(None) => {
                    let has_data = self.cols.cols.iter().any(|c| !c.is_empty());
                    if has_data {
                        self.len = self.len.saturating_sub(1);
                        return self.finish_batch();
                    }
                    self.len = 0;
                    return None;
                }
                Err(error) => return self.stop_with_error(error),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<const STRICT_SCHEMA: bool> ExactSizeIterator for DataFrameIter<STRICT_SCHEMA> {}
impl<const STRICT_SCHEMA: bool> FusedIterator for DataFrameIter<STRICT_SCHEMA> {}

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
    DataFrameIter::new(
        batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, false, None,
    )
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
    config: Option<crate::sheet_fast::FastConfig>,
) -> anyhow::Result<DataFrameIter> {
    DataFrameIter::new(
        batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, true, config,
    )
}

/// 固定 Schema 的低内存流式读取。
pub fn df_iter_with_schema(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
    schema: SchemaRef,
) -> anyhow::Result<StrictDataFrameIter> {
    StrictDataFrameIter::new_with_schema(
        batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, schema, false, None,
    )
}

/// 固定 Schema 的低内存流式读取，并应用严格读取选项。
pub fn df_iter_with_schema_and_options(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
    schema: SchemaRef,
    options: StrictReadOptions,
) -> anyhow::Result<StrictDataFrameIter> {
    StrictDataFrameIter::new_with_schema_and_options(
        batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, schema, options, false,
        None,
    )
}

/// 固定 Schema 的 fast 流式读取。
///
/// 表头只映射一次，逐单元格路径使用预分配原生 builder，不做类型推断、升级或
/// AnyValue 回退；适合作为 ETL 严格读取阶段。
pub fn df_iter_fast_with_schema(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
    schema: SchemaRef,
    config: Option<crate::sheet_fast::FastConfig>,
) -> anyhow::Result<StrictDataFrameIter> {
    StrictDataFrameIter::new_with_schema(
        batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, schema, true, config,
    )
}

/// 固定 Schema 的 fast 流式读取，并应用严格读取选项。
pub fn df_iter_fast_with_schema_and_options(
    batch_size: Option<usize>,
    path: impl AsRef<Path>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<&[u32]>,
    schema: SchemaRef,
    options: StrictReadOptions,
    config: Option<crate::sheet_fast::FastConfig>,
) -> anyhow::Result<StrictDataFrameIter> {
    StrictDataFrameIter::new_with_schema_and_options(
        batch_size, path, sheet_name, sheet_idx, has_header, skip_rows, schema, options, true,
        config,
    )
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
mod strict_schema_tests {
    use super::*;

    fn schema(fields: impl IntoIterator<Item = (&'static str, DataType)>) -> SchemaRef {
        Arc::new(
            fields
                .into_iter()
                .map(|(name, dtype)| (PlSmallStr::from_static(name), dtype))
                .collect(),
        )
    }

    #[test]
    fn strict_columns_compile_header_mapping_once_and_keep_schema_order() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 1),
        };
        let mut cols = TypedCols::new(&dimensions, 8);
        cols.headers = vec!["text".to_string(), "id".to_string()];
        let expected = schema([("id", DataType::Int64), ("text", DataType::String)]);

        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        assert_eq!(mapping, vec![1, 0]);
        assert!(
            cols.cols
                .iter()
                .all(|col| !matches!(col, TypedCol::AnyValue(_)))
        );

        cols.push_cell_strict(
            Cell::new((1, 0), Data::String(PlSmallStr::from_static("hello"))),
            0,
            &mapping,
            false,
        )?;
        cols.push_cell_strict(Cell::new((1, 1), Data::Int(42)), 0, &mapping, false)?;
        let frame = cols.finish_dataframe_strict(1)?;

        assert_eq!(
            frame.get_column_names_owned(),
            vec![
                PlSmallStr::from_static("id"),
                PlSmallStr::from_static("text")
            ]
        );
        assert_eq!(frame.dtypes(), vec![DataType::Int64, DataType::String]);
        assert_eq!(frame.column("id")?.i64()?.get(0), Some(42));
        assert_eq!(frame.column("text")?.str()?.get(0), Some("hello"));
        Ok(())
    }

    #[test]
    fn strict_columns_reject_value_type_mismatch_and_duplicate_headers() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 1),
        };
        let expected = schema([("id", DataType::Int64), ("name", DataType::String)]);

        let mut duplicate = TypedCols::new(&dimensions, 8);
        duplicate.headers = vec!["id".to_string(), "id".to_string()];
        let error = duplicate
            .configure_strict_schema(expected.as_ref(), true)
            .unwrap_err();
        let typed = error.downcast_ref::<StrictReadError>().unwrap();
        assert!(matches!(
            &typed.kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::DuplicateHeader { name })
                if name == "id"
        ));
        assert!(error.to_string().contains("重复"));

        let mut cols = TypedCols::new(&dimensions, 8);
        cols.headers = vec!["id".to_string(), "name".to_string()];
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        let error = cols
            .push_cell_strict(
                Cell::new((1, 0), Data::String(PlSmallStr::from_static("not-an-id"))),
                0,
                &mapping,
                false,
            )
            .unwrap_err();
        let typed = error.downcast_ref::<StrictReadError>().unwrap();
        assert!(matches!(
            &typed.kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::CellPhysicalType {
                cell,
                column,
                expected,
                actual,
            }) if cell == "A2"
                && column == "id"
                && expected == "Int64"
                && actual == "String"
        ));
        assert!(error.to_string().contains("A2"));
        assert!(error.to_string().contains("期望 Int64"));
        Ok(())
    }

    #[test]
    fn strict_float64_rejects_unrepresentable_integer_and_extreme_lexemes() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 0),
        };
        let expected = schema([("amount", DataType::Float64)]);
        for (data, raw) in [
            (Data::Int(9_007_199_254_740_993), "9007199254740993"),
            (Data::Float(9_007_199_254_740_992.0), "9007199254740993"),
            (Data::Float(f64::INFINITY), "1e400"),
            (Data::Float(0.0), "1e-400"),
        ] {
            let mut cols = TypedCols::new(&dimensions, 1);
            cols.headers = vec!["amount".into()];
            let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
            let error = cols
                .push_cell_strict(
                    Cell::with_raw_numeric_lexeme((1, 0), data, Some(raw.into())),
                    0,
                    &mapping,
                    false,
                )
                .unwrap_err();
            let typed = error.downcast_ref::<StrictReadError>().unwrap();
            assert!(matches!(
                &typed.kind,
                StrictReadErrorKind::SchemaMismatch(SchemaMismatch::Float64LexemeLoss { .. })
            ));
        }

        let mut cols = TypedCols::new(&dimensions, 1);
        cols.headers = vec!["amount".into()];
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        cols.push_cell_strict(
            Cell::with_raw_numeric_lexeme((1, 0), Data::Float(0.1), Some("0.1".into())),
            0,
            &mapping,
            false,
        )?;
        assert_eq!(
            cols.finish_dataframe_strict(1)?
                .column("amount")?
                .f64()?
                .get(0),
            Some(0.1)
        );
        Ok(())
    }

    #[test]
    fn exact_decimal_parser_preserves_scale_without_implicit_rounding() {
        assert_eq!(exact_decimal_coefficient("1.2300", 5, 2), Some(123));
        assert_eq!(exact_decimal_coefficient("-0.00123", 6, 5), Some(-123));
        assert_eq!(exact_decimal_coefficient("1.2e3", 6, 2), Some(120_000));
        assert_eq!(exact_decimal_coefficient("00123", 5, 0), Some(123));
        assert_eq!(exact_decimal_coefficient("0e999999", 1, 0), Some(0));
        assert_eq!(exact_decimal_coefficient("1.2300", 3, 1), None);
        assert_eq!(exact_decimal_coefficient("1e-39", 38, 38), None);
        assert_eq!(exact_decimal_coefficient("1e39", 38, 0), None);
        assert_eq!(exact_decimal_coefficient("NaN", 5, 2), None);
    }

    #[test]
    fn strict_decimal_uses_raw_numeric_lexemes_and_null_bitmap() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (4, 0),
        };
        let expected = schema([("amount", DataType::Decimal(20, 2))]);
        let mut cols = TypedCols::new(&dimensions, 4);
        cols.headers = vec!["amount".into()];
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        cols.push_cell_strict(
            Cell::with_raw_numeric_lexeme((1, 0), Data::Float(1.23), Some("1.2300".into())),
            0,
            &mapping,
            false,
        )?;
        cols.push_cell_strict(
            Cell::with_raw_numeric_lexeme(
                (3, 0),
                Data::Float(9_007_199_254_740_992.0),
                Some("9007199254740993".into()),
            ),
            2,
            &mapping,
            false,
        )?;
        cols.push_cell_strict(
            Cell::with_raw_numeric_lexeme((4, 0), Data::Int(-2), Some("-2".into())),
            3,
            &mapping,
            false,
        )?;
        let frame = cols.finish_dataframe_strict(4)?;
        let amount = frame.column("amount")?.decimal()?;
        assert_eq!(amount.dtype(), &DataType::Decimal(20, 2));
        assert_eq!(amount.physical().get(0), Some(123));
        assert_eq!(amount.physical().get(1), None);
        assert_eq!(amount.physical().get(2), Some(900_719_925_474_099_300));
        assert_eq!(amount.physical().get(3), Some(-200));
        Ok(())
    }

    #[test]
    fn strict_decimal_rejects_precision_or_scale_loss() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 0),
        };
        for (precision, scale, raw) in [(5, 2, "1.234"), (5, 2, "1234.56")] {
            let mut cols = TypedCols::new(&dimensions, 1);
            cols.headers = vec!["amount".into()];
            let expected = schema([("amount", DataType::Decimal(precision, scale))]);
            let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
            let error = cols
                .push_cell_strict(
                    Cell::with_raw_numeric_lexeme((1, 0), Data::Float(1.234), Some(raw.into())),
                    0,
                    &mapping,
                    false,
                )
                .unwrap_err();
            let typed = error.downcast_ref::<StrictReadError>().unwrap();
            assert!(matches!(
                &typed.kind,
                StrictReadErrorKind::SchemaMismatch(SchemaMismatch::DecimalLexemeLoss { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn strict_string_uses_deterministic_scalar_representations() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (6, 0),
        };
        let expected = schema([("value", DataType::String)]);
        let mut cols = TypedCols::new(&dimensions, 6);
        cols.headers = vec!["value".into()];
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        let cells = [
            Cell::with_raw_numeric_lexeme((1, 0), Data::Int(123), Some("00123".into())),
            Cell::with_raw_numeric_lexeme((2, 0), Data::Float(1.23), Some("1.2300".into())),
            Cell::with_raw_numeric_lexeme(
                (3, 0),
                Data::Float(9_007_199_254_740_992.0),
                Some("9007199254740993".into()),
            ),
            Cell::new((4, 0), Data::Bool(true)),
            Cell::new(
                (5, 0),
                Data::DateTime(crate::excel_types::ExcelDateTime::new(44927.5, false)),
            ),
            Cell::new((6, 0), Data::String(PlSmallStr::from_static("00123"))),
        ];
        for (row, cell) in cells.into_iter().enumerate() {
            cols.push_cell_strict(cell, row, &mapping, false)?;
        }
        let frame = cols.finish_dataframe_strict(6)?;
        let values = frame.column("value")?.str()?;
        assert_eq!(values.get(0), Some("123"));
        assert_eq!(values.get(1), Some("1.23"));
        assert_eq!(values.get(2), Some("9007199254740993"));
        assert_eq!(values.get(3), Some("true"));
        assert_eq!(values.get(4), Some("2023-01-01T12:00:00"));
        assert_eq!(values.get(5), Some("00123"));
        Ok(())
    }

    #[test]
    fn strict_temporal_reads_calendar_and_target_units_without_nanos_intermediate()
    -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 0),
        };
        for (is_1904, serial) in [(false, 25_569.5), (true, 24_107.5)] {
            for (unit, expected) in [
                (TimeUnit::Milliseconds, 43_200_000),
                (TimeUnit::Microseconds, 43_200_000_000),
                (TimeUnit::Nanoseconds, 43_200_000_000_000),
            ] {
                let mut cols = TypedCols::new(&dimensions, 1);
                cols.headers = vec!["created_at".into()];
                let expected_schema = schema([("created_at", DataType::Datetime(unit, None))]);
                let mapping = cols.configure_strict_schema(expected_schema.as_ref(), true)?;
                cols.push_cell_strict(
                    Cell::new(
                        (1, 0),
                        Data::DateTime(crate::excel_types::ExcelDateTime::new(serial, is_1904)),
                    ),
                    0,
                    &mapping,
                    is_1904,
                )?;
                let frame = cols.finish_dataframe_strict(1)?;
                assert_eq!(
                    frame.column("created_at")?.datetime()?.physical().get(0),
                    Some(expected)
                );
            }
        }
        for (unit, expected) in [
            (TimeUnit::Milliseconds, 253_402_214_400_000),
            (TimeUnit::Microseconds, 253_402_214_400_000_000),
        ] {
            let mut cols = TypedCols::new(&dimensions, 1);
            cols.headers = vec!["created_at".into()];
            let expected_schema = schema([("created_at", DataType::Datetime(unit, None))]);
            let mapping = cols.configure_strict_schema(expected_schema.as_ref(), true)?;
            cols.push_cell_strict(Cell::new((1, 0), Data::Int(2_958_465)), 0, &mapping, false)?;
            assert_eq!(
                cols.finish_dataframe_strict(1)?
                    .column("created_at")?
                    .datetime()?
                    .physical()
                    .get(0),
                Some(expected)
            );
        }
        Ok(())
    }

    #[test]
    fn strict_temporal_rejects_nonzero_time_range_loss_and_text_parsing() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 0),
        };
        for (dtype, data, reason) in [
            (
                DataType::Date,
                Data::Float(25_569.5),
                "date_has_nonzero_time",
            ),
            (
                DataType::Date,
                Data::Float(f64::INFINITY),
                "temporal_out_of_range",
            ),
            (
                DataType::Datetime(TimeUnit::Nanoseconds, None),
                Data::Int(2_958_465),
                "temporal_out_of_range",
            ),
        ] {
            let mut cols = TypedCols::new(&dimensions, 1);
            cols.headers = vec!["date".into()];
            let expected = schema([("date", dtype)]);
            let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
            let error = cols
                .push_cell_strict(Cell::new((1, 0), data), 0, &mapping, false)
                .unwrap_err();
            assert!(matches!(
                &error.downcast_ref::<StrictReadError>().unwrap().kind,
                StrictReadErrorKind::SchemaMismatch(SchemaMismatch::TemporalValueMismatch { reason: actual, .. }) if actual == reason
            ));
        }
        let mut cols = TypedCols::new(&dimensions, 1);
        cols.headers = vec!["date".into()];
        let expected = schema([("date", DataType::Date)]);
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        let error = cols
            .push_cell_strict(
                Cell::new((1, 0), Data::String("2026-09-28".into())),
                0,
                &mapping,
                false,
            )
            .unwrap_err();
        assert!(matches!(
            &error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::CellPhysicalType { actual, .. }) if actual == "String"
        ));
        Ok(())
    }

    #[test]
    fn strict_date_uses_unix_days_directly_for_far_years() -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 0),
        };
        let mut cols = TypedCols::new(&dimensions, 1);
        cols.headers = vec!["date".into()];
        let expected = schema([("date", DataType::Date)]);
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        cols.push_cell_strict(Cell::new((1, 0), Data::Int(2_958_465)), 0, &mapping, false)?;
        assert_eq!(
            cols.finish_dataframe_strict(1)?
                .column("date")?
                .date()?
                .physical()
                .get(0),
            Some(2_932_896)
        );
        Ok(())
    }

    #[test]
    fn strict_schema_errors_downcast_with_header_details_and_separate_excel_errors()
    -> anyhow::Result<()> {
        let dimensions = Dimensions {
            start: (0, 0),
            end: (1, 1),
        };
        let expected = schema([("id", DataType::Int64), ("name", DataType::String)]);

        let mut header_mismatch = TypedCols::new(&dimensions, 8);
        header_mismatch.headers = vec!["id".to_string(), "other".to_string()];
        let error = header_mismatch
            .configure_strict_schema(expected.as_ref(), true)
            .unwrap_err();
        assert!(matches!(
            &error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::HeaderColumns { missing, extra })
                if missing == &["name"] && extra == &["other"]
        ));

        let mut headerless_count = TypedCols::new(&dimensions, 8);
        headerless_count.headers = vec!["col_0".to_string()];
        let error = headerless_count
            .configure_strict_schema(expected.as_ref(), false)
            .unwrap_err();
        assert!(matches!(
            &error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::HeaderlessColumnCount {
                actual: 1,
                expected: 2,
            })
        ));

        let mut cols = TypedCols::new(&dimensions, 8);
        cols.headers = vec!["id".to_string(), "name".to_string()];
        let mapping = cols.configure_strict_schema(expected.as_ref(), true)?;
        let error = cols
            .push_cell_strict(
                Cell::new((1, 0), Data::Error(crate::excel_types::CellErrorType::Div0)),
                0,
                &mapping,
                false,
            )
            .unwrap_err();
        assert!(matches!(
            &error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::ExcelCellError { cell, column, error }
                if cell == "A2" && column == "id" && error == "#DIV/0!"
        ));
        assert!(!matches!(
            error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::SchemaMismatch(_)
        ));
        Ok(())
    }

    #[test]
    fn fast_strict_reader_streams_reordered_columns_across_batches() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let expected = schema([
            ("String", DataType::String),
            ("Int", DataType::Int64),
            ("DateTime", DataType::Datetime(TimeUnit::Microseconds, None)),
            ("Float", DataType::Float64),
            ("Bool", DataType::Boolean),
            ("DurationIso", DataType::String),
            ("Empty", DataType::String),
        ]);
        let mut iter = df_iter_fast_with_schema(
            Some(5),
            path,
            Some("Sheet1"),
            None,
            true,
            None,
            Arc::clone(&expected),
            None,
        )?;

        for _ in 0..2 {
            let frame = iter.next().expect("应至少产生两个 batch")?;
            assert_eq!(frame.height(), 5);
            assert_eq!(frame.schema().as_ref(), expected.as_ref());
        }
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
        let mut iter = DataFrameIter::from_workbook(
            Some(5),
            Arc::clone(&wb),
            Some("Sheet1"),
            None,
            true,
            None,
            false,
            None,
        )?;

        let df1 = iter.next().unwrap()?;
        let rows1 = df1.height();
        println!(
            "Sheet1 first batch: {} rows, cols: {:?}",
            rows1,
            df1.get_column_names()
        );

        iter.select_sheet(Some("Sheet2"), None)?;
        let df2 = iter.next().unwrap()?;
        let rows2 = df2.height();
        println!(
            "Sheet2 first batch: {} rows, cols: {:?}",
            rows2,
            df2.get_column_names()
        );

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
        if let Some(batch) = iter.into_iter().next() {
            let df = batch?;
            println!("first batch: {} rows", df.height());
            assert_eq!(
                df.height(),
                5,
                "first batch should have 5 rows (row 1 skipped before batch)"
            );
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
        if let Some(batch) = iter.into_iter().next() {
            let df = batch?;
            println!("first batch: {} rows", df.height());
            // Batch reads raw rows 1,2,3,4,5,6,7,8 (needs 5 valid rows)
            // Skip 2,3,4. Valid: 1,5,6,7,8 → 5 rows
            assert_eq!(df.height(), 5);
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

#[cfg(test)]
mod skip_top_rows_tests {
    use std::{
        fs::File,
        io::Write,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use ::zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;

    struct XlsxFixture(PathBuf);

    impl Drop for XlsxFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn fixture() -> anyhow::Result<XlsxFixture> {
        fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B4"/><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row><row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2" t="s"><v>2</v></c></row><row r="3"><c r="A3"><v>7</v></c><c r="B3" t="s"><v>3</v></c></row><row r="4"><c r="A4"><v>8</v></c><c r="B4" t="s"><v>4</v></c></row></sheetData></worksheet>"#,
        )
    }

    fn empty_fixture() -> anyhow::Result<XlsxFixture> {
        fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B1"/><sheetData></sheetData></worksheet>"#,
        )
    }

    fn type_mismatch_fixture() -> anyhow::Result<XlsxFixture> {
        fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B2"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c><c r="B1" t="s"><v>2</v></c></row><row r="2"><c r="A2" t="s"><v>3</v></c><c r="B2" t="s"><v>4</v></c></row></sheetData></worksheet>"#,
        )
    }

    fn excel_error_fixture() -> anyhow::Result<XlsxFixture> {
        fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B2"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c><c r="B1" t="s"><v>2</v></c></row><row r="2"><c r="A2" t="e"><v>#DIV/0!</v></c><c r="B2" t="s"><v>3</v></c></row></sheetData></worksheet>"#,
        )
    }

    fn fixture_with_sheet_xml(sheet_xml: &str) -> anyhow::Result<XlsxFixture> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "stream-xlsx-skip-top-rows-{}-{id}.xlsx",
            std::process::id()
        ));
        let file = File::create(&path)?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let entries = [
            (
                "xl/workbook.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="5" uniqueCount="5"><si><t>preface</t></si><si><t>id</t></si><si><t>name</t></si><si><t>alice</t></si><si><t>bob</t></si></sst>"#,
            ),
            (
                "xl/styles.xml",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14"/></cellXfs></styleSheet>"#,
            ),
            ("xl/worksheets/sheet1.xml", sheet_xml),
        ];
        for (name, contents) in entries {
            zip.start_file(name, options.clone())?;
            zip.write_all(contents.as_bytes())?;
        }
        zip.finish()?;
        Ok(XlsxFixture(path))
    }

    fn schema() -> SchemaRef {
        Arc::new(
            [
                (PlSmallStr::from_static("id"), DataType::Int64),
                (PlSmallStr::from_static("name"), DataType::String),
            ]
            .into_iter()
            .collect(),
        )
    }

    #[test]
    fn strict_skip_top_rows_precedes_header_discovery() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let iter = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            true,
            None,
            schema(),
            StrictReadOptions { skip_top_rows: 1 },
        )?;
        let batches = iter.collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(batches.len(), 1);
        let frame = &batches[0];
        assert_eq!(frame.height(), 2);
        assert_eq!(frame.column("id")?.i64()?.get(0), Some(7));
        assert_eq!(frame.column("id")?.i64()?.get(1), Some(8));
        assert_eq!(frame.column("name")?.str()?.get(0), Some("alice"));
        assert_eq!(frame.column("name")?.str()?.get(1), Some("bob"));
        Ok(())
    }

    #[test]
    fn strict_skip_top_rows_precedes_first_headerless_data_row() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let iter = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            false,
            None,
            schema(),
            StrictReadOptions { skip_top_rows: 2 },
        )?;
        let batches = iter.collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(batches.len(), 1);
        let frame = &batches[0];
        assert_eq!(frame.height(), 2);
        assert_eq!(frame.column("id")?.i64()?.get(0), Some(7));
        assert_eq!(frame.column("id")?.i64()?.get(1), Some(8));
        assert_eq!(frame.column("name")?.str()?.get(0), Some("alice"));
        assert_eq!(frame.column("name")?.str()?.get(1), Some("bob"));
        Ok(())
    }

    #[test]
    fn strict_skip_top_rows_composes_with_existing_skip_rows() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let iter = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            true,
            Some(&[2]),
            schema(),
            StrictReadOptions { skip_top_rows: 1 },
        )?;
        let batches = iter.collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(batches.len(), 1);
        let frame = &batches[0];
        assert_eq!(frame.height(), 1);
        assert_eq!(frame.column("id")?.i64()?.get(0), Some(8));
        assert_eq!(frame.column("name")?.str()?.get(0), Some("bob"));
        Ok(())
    }

    #[test]
    fn strict_skip_all_physical_rows_requires_header_when_header_enabled() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let header_error = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            true,
            None,
            schema(),
            StrictReadOptions { skip_top_rows: 10 },
        )
        .err();
        assert!(header_error.is_some_and(|error| error.to_string().contains("缺少表头")));

        let iter = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            false,
            None,
            schema(),
            StrictReadOptions { skip_top_rows: 10 },
        )?;
        assert_eq!(iter.count(), 0);
        Ok(())
    }

    #[test]
    fn strict_empty_sheet_requires_header_but_keeps_headerless_empty_path() -> anyhow::Result<()> {
        let fixture = empty_fixture()?;
        let error = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            true,
            None,
            schema(),
            StrictReadOptions::default(),
        )
        .err()
        .expect("要求表头时空工作表应报 MissingHeader");
        let typed = error.downcast_ref::<StrictReadError>().unwrap();
        assert!(matches!(
            &typed.kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::MissingHeader { .. })
        ));
        assert_eq!(typed.sheet.as_deref(), Some("Sheet1"));
        assert!(error.to_string().contains("缺少表头"));

        let iter = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            false,
            None,
            schema(),
            StrictReadOptions::default(),
        )?;
        assert_eq!(iter.count(), 0);
        Ok(())
    }

    #[test]
    fn strict_date_checks_original_serial_on_slow_and_fast_paths() -> anyhow::Result<()> {
        for raw in ["44927.000000000001", "1e-400"] {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A2"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2" s="1"><v>{raw}</v></c></row></sheetData></worksheet>"#
            );
            let fixture = fixture_with_sheet_xml(&xml)?;
            let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
            let mut source = XlsxStreamReader::from_workbook(workbook, Some("Sheet1"), None)?
                .with_numeric_lexemes(true);
            source.next_cell()?;
            let date_cell = source.next_cell()?.unwrap();
            assert!(matches!(date_cell.get_value(), Data::DateTime(_)));
            assert_eq!(date_cell.raw_numeric_lexeme(), Some(raw));
            for fast in [false, true] {
                let expected = Arc::new(
                    [(PlSmallStr::from_static("id"), DataType::Date)]
                        .into_iter()
                        .collect(),
                );
                let reader = StrictDataFrameIter::new_with_schema_and_options(
                    Some(8),
                    &fixture.0,
                    Some("Sheet1"),
                    None,
                    true,
                    None,
                    expected,
                    StrictReadOptions::default(),
                    fast,
                    None,
                )?;
                let error = reader.collect::<anyhow::Result<Vec<_>>>().unwrap_err();
                assert!(
                    matches!(
                        &error.downcast_ref::<StrictReadError>().unwrap().kind,
                        StrictReadErrorKind::SchemaMismatch(SchemaMismatch::TemporalValueMismatch { reason, .. })
                            if reason == "date_serial_not_exact_integer"
                    ),
                    "raw={raw}, fast={fast}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn strict_date_accepts_exact_integer_serial_and_rejects_date_text() -> anyhow::Result<()> {
        for (value, accepted) in [
            (r#"s="1"><v>44927.0000000000000</v>"#, true),
            (r#"s="1"><v>4.4927e4</v>"#, true),
            (r#"t="str"><v>2023-01-01</v>"#, false),
            (r#"t="d"><v>2023-01-01T00:00:00</v>"#, false),
        ] {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A2"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2" {value}</c></row></sheetData></worksheet>"#
            );
            let fixture = fixture_with_sheet_xml(&xml)?;
            for fast in [false, true] {
                let expected = Arc::new(
                    [(PlSmallStr::from_static("id"), DataType::Date)]
                        .into_iter()
                        .collect(),
                );
                let reader = StrictDataFrameIter::new_with_schema_and_options(
                    Some(8),
                    &fixture.0,
                    Some("Sheet1"),
                    None,
                    true,
                    None,
                    expected,
                    StrictReadOptions::default(),
                    fast,
                    None,
                )?;
                let result = reader.collect::<anyhow::Result<Vec<_>>>();
                if accepted {
                    let frames = result?;
                    assert_eq!(
                        frames[0].column("id")?.date()?.physical().get(0),
                        Some(19_358)
                    );
                } else {
                    assert!(matches!(
                        &result
                            .unwrap_err()
                            .downcast_ref::<StrictReadError>()
                            .unwrap()
                            .kind,
                        StrictReadErrorKind::SchemaMismatch(
                            SchemaMismatch::CellPhysicalType { .. }
                        )
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn full_sheet_scan_sees_late_type_and_sparse_null_without_buffering_rows() -> anyhow::Result<()>
    {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B1000001"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c><c r="B1" t="s"><v>2</v></c></row><row r="2"><c r="A2"><v>7</v></c><c r="B2" t="s"><v>3</v></c></row><row r="1000001"><c r="A1000001"><v>9.5</v></c></row></sheetData></worksheet>"#,
        )?;
        let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
        let observed = inspect_sheet_schema_from_workbook(
            workbook,
            Some("Sheet1"),
            None,
            true,
            None,
            StrictReadOptions::default(),
        )?;
        assert_eq!(observed.row_count, 2);
        assert_eq!(observed.fields.len(), 2);
        assert_eq!(observed.fields[0].name, "id");
        assert_eq!(observed.fields[0].inferred_type, DataType::Float64);
        assert_eq!(
            observed.fields[0].physical_kinds,
            [SheetPhysicalKind::Int64, SheetPhysicalKind::Float64]
                .into_iter()
                .collect()
        );
        assert_eq!(observed.fields[0].null_count, 0);
        assert!(!observed.fields[0].requires_exact_numeric_review);
        assert_eq!(observed.fields[1].name, "name");
        assert_eq!(observed.fields[1].inferred_type, DataType::String);
        assert_eq!(observed.fields[1].null_count, 1);
        Ok(())
    }

    #[test]
    fn full_sheet_scan_flags_inexact_int_float_mix_and_missing_header() -> anyhow::Result<()> {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A3"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2"><v>9007199254740993</v></c></row><row r="3"><c r="A3"><v>0.5</v></c></row></sheetData></worksheet>"#,
        )?;
        let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
        let observed = inspect_sheet_schema_from_workbook(
            workbook,
            Some("Sheet1"),
            None,
            true,
            None,
            StrictReadOptions::default(),
        )?;
        assert_eq!(observed.fields[0].inferred_type, DataType::Decimal(17, 1));
        assert!(observed.fields[0].requires_exact_numeric_review);
        assert!(!observed.fields[0].unsupported_exact_numeric);

        let empty = empty_fixture()?;
        let workbook = Arc::new(XlsxWorkbook::open(&empty.0)?);
        let error = inspect_sheet_schema_from_workbook(
            workbook,
            Some("Sheet1"),
            None,
            true,
            None,
            StrictReadOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(
            &error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::MissingHeader { .. })
        ));
        Ok(())
    }

    #[test]
    fn full_sheet_decimal_candidate_can_be_read_exactly_from_the_same_xml() -> anyhow::Result<()> {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A3"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2"><v>9007199254740993</v></c></row><row r="3"><c r="A3"><v>0.5</v></c></row></sheetData></worksheet>"#,
        )?;
        let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
        let observed = inspect_sheet_schema_from_workbook(
            workbook,
            Some("Sheet1"),
            None,
            true,
            None,
            StrictReadOptions::default(),
        )?;
        assert_eq!(observed.fields[0].inferred_type, DataType::Decimal(17, 1));

        let schema = Arc::new(
            [(
                PlSmallStr::from_static("id"),
                observed.fields[0].inferred_type.clone(),
            )]
            .into_iter()
            .collect(),
        );
        let batches = df_iter_with_schema_and_options(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            true,
            None,
            schema,
            StrictReadOptions::default(),
        )?
        .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(batches.len(), 1);
        let values = batches[0].column("id")?.decimal()?.physical();
        assert_eq!(values.get(0), Some(90_071_992_547_409_930));
        assert_eq!(values.get(1), Some(5));
        Ok(())
    }

    #[test]
    fn full_sheet_scan_flags_numeric_lexeme_beyond_decimal128() -> anyhow::Result<()> {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A3"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2"><v>9007199254740993</v></c></row><row r="3"><c r="A3"><v>1e-39</v></c></row></sheetData></worksheet>"#,
        )?;
        let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
        let observed = inspect_sheet_schema_from_workbook(
            workbook,
            Some("Sheet1"),
            None,
            true,
            None,
            StrictReadOptions::default(),
        )?;
        assert!(observed.fields[0].requires_exact_numeric_review);
        assert!(observed.fields[0].unsupported_exact_numeric);
        assert_eq!(observed.fields[0].inferred_type, DataType::Float64);
        Ok(())
    }

    #[test]
    fn numeric_decimal_width_handles_exponents_and_trailing_zeroes() {
        assert_eq!(numeric_decimal_width("1.2300"), Some((1, 2)));
        assert_eq!(numeric_decimal_width("-0.00123"), Some((0, 5)));
        assert_eq!(numeric_decimal_width("1.2e3"), Some((4, 0)));
        assert_eq!(numeric_decimal_width("0e999999"), Some((0, 0)));
        assert_eq!(numeric_decimal_width("1e-39"), None);
        assert_eq!(numeric_decimal_width("1e39"), None);
        assert_eq!(numeric_decimal_width("NaN"), None);
    }

    #[test]
    fn full_sheet_scan_rejects_float_overflow_and_nonzero_underflow() -> anyhow::Result<()> {
        for numeric in ["1e400", "1e-400"] {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A2"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2"><v>{numeric}</v></c></row></sheetData></worksheet>"#
            );
            let fixture = fixture_with_sheet_xml(&xml)?;
            let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
            let observed = inspect_sheet_schema_from_workbook(
                workbook,
                Some("Sheet1"),
                None,
                true,
                None,
                StrictReadOptions::default(),
            )?;
            assert!(
                observed.fields[0].requires_exact_numeric_review,
                "{numeric}"
            );
            assert!(observed.fields[0].unsupported_exact_numeric, "{numeric}");
        }
        Ok(())
    }

    #[test]
    fn full_sheet_scan_keeps_numeric_looking_text_as_text() -> anyhow::Result<()> {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A3"/><sheetData><row r="1"><c r="A1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="str"><v>00123</v></c></row><row r="3"><c r="A3"><v>123</v></c></row></sheetData></worksheet>"#,
        )?;
        let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
        let observed = inspect_sheet_schema_from_workbook(
            workbook,
            Some("Sheet1"),
            None,
            true,
            None,
            StrictReadOptions::default(),
        )?;
        assert_eq!(observed.fields[0].inferred_type, DataType::String);
        assert_eq!(
            observed.fields[0].physical_kinds,
            [SheetPhysicalKind::Int64, SheetPhysicalKind::Text]
                .into_iter()
                .collect()
        );
        Ok(())
    }

    #[test]
    fn slow_reader_preserves_numeric_lexemes_without_retyping_text() -> anyhow::Result<()> {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A6"/><sheetData><row r="1"><c r="A1" t="str"><v>00123</v></c></row><row r="2"><c r="A2"><v>00123</v></c></row><row r="3"><c r="A3"><v>1.2300</v></c></row><row r="4"><c r="A4"><v>1e3</v></c></row><row r="5"><c r="A5" t="b"><v>1</v></c></row><row r="6"><c r="A6" t="e"><v>#N/A</v></c></row></sheetData></worksheet>"#,
        )?;
        let workbook = Arc::new(XlsxWorkbook::open(&fixture.0)?);
        let mut default_reader =
            XlsxStreamReader::from_workbook(Arc::clone(&workbook), Some("Sheet1"), None)?;
        default_reader.next_cell()?;
        assert_eq!(
            default_reader
                .next_cell()?
                .expect("应有数值单元格")
                .raw_numeric_lexeme(),
            None,
            "普通读取不应为每个数值单元格增加词法分配"
        );
        let mut reader = XlsxStreamReader::from_workbook(workbook, Some("Sheet1"), None)?
            .with_numeric_lexemes(true);
        let mut cells = Vec::new();
        while let Some(cell) = reader.next_cell()? {
            cells.push(cell);
        }
        assert_eq!(cells.len(), 6);
        assert!(matches!(cells[0].get_value(), Data::String(value) if value.as_str() == "00123"));
        assert_eq!(cells[0].raw_numeric_lexeme(), None);
        assert_eq!(cells[1].get_value(), &Data::Int(123));
        assert_eq!(cells[1].raw_numeric_lexeme(), Some("00123"));
        assert_eq!(cells[2].get_value(), &Data::Float(1.23));
        assert_eq!(cells[2].raw_numeric_lexeme(), Some("1.2300"));
        assert_eq!(cells[3].get_value(), &Data::Float(1_000.0));
        assert_eq!(cells[3].raw_numeric_lexeme(), Some("1e3"));
        assert_eq!(cells[4].raw_numeric_lexeme(), None);
        assert_eq!(cells[5].raw_numeric_lexeme(), None);
        Ok(())
    }

    #[test]
    fn truncated_sheet_xml_is_not_treated_as_a_complete_dataset() -> anyhow::Result<()> {
        let fixture = fixture_with_sheet_xml(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:B1"/><sheetData><row r="1"><c r="A1"><v>7</v></c><c r="B1" t="s"><v>3</v></c></row>"#,
        )?;
        let iter = df_iter_with_schema(
            Some(8),
            &fixture.0,
            Some("Sheet1"),
            None,
            false,
            None,
            schema(),
        )?;
        let error = iter.collect::<anyhow::Result<Vec<_>>>().unwrap_err();
        assert!(error.to_string().contains("sheetData 结束前截断"));
        Ok(())
    }

    #[test]
    fn strict_initial_header_mismatch_has_selected_sheet_index() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let error = df_iter_with_schema(Some(8), &fixture.0, None, Some(0), true, None, schema())
            .err()
            .expect("表头字段不匹配应在 strict reader 初始化时失败");
        let typed = error.downcast_ref::<StrictReadError>().unwrap();
        assert!(matches!(
            &typed.kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::HeaderColumns { .. })
        ));
        assert_eq!(typed.sheet.as_deref(), Some("#0"));
        assert!(error.to_string().starts_with("工作表 '#0'："));
        Ok(())
    }

    #[test]
    fn strict_reader_preserves_typed_errors_with_sheet_context() -> anyhow::Result<()> {
        let mismatch = type_mismatch_fixture()?;
        let mut iter = df_iter_with_schema(
            Some(8),
            &mismatch.0,
            Some("Sheet1"),
            None,
            true,
            None,
            schema(),
        )?;
        let error = iter
            .next()
            .expect("类型不匹配应产生一条错误")
            .err()
            .expect("类型不匹配不得成功产出 DataFrame");
        let typed = error.downcast_ref::<StrictReadError>().unwrap();
        assert_eq!(typed.sheet.as_deref(), Some("Sheet1"));
        assert!(matches!(
            &typed.kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::CellPhysicalType {
                cell,
                column,
                actual,
                ..
            }) if cell == "A2" && column == "id" && actual == "String"
        ));
        assert!(error.to_string().starts_with("工作表 'Sheet1'："));

        let excel_error = excel_error_fixture()?;
        let mut iter = df_iter_with_schema(
            Some(8),
            &excel_error.0,
            Some("Sheet1"),
            None,
            true,
            None,
            schema(),
        )?;
        let error = iter
            .next()
            .expect("Excel Error 应产生一条错误")
            .err()
            .expect("Excel Error 不得成功产出 DataFrame");
        let typed = error.downcast_ref::<StrictReadError>().unwrap();
        assert!(matches!(
            &typed.kind,
            StrictReadErrorKind::ExcelCellError { cell, error, .. }
                if cell == "A2" && error == "#DIV/0!"
        ));
        assert_eq!(typed.sheet.as_deref(), Some("Sheet1"));
        Ok(())
    }
}
