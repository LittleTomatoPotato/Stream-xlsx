use anyhow::Result as aResult;
use calamine::{Cell, DataRef, ExcelDateTime, Reader, Sheets, open_workbook_auto};
use chrono::{NaiveDate, NaiveDateTime};
use polars::prelude::*;
use std::iter::from_fn;

fn main() -> aResult<()> {
    let wb = open_workbook_auto("test_data.xlsx")?;
    let mut xlsx = match wb {
        Sheets::Xlsx(sheet) => sheet,
        _ => return Err(anyhow::anyhow!("非xlsx")),
    };
    let metadata = xlsx.metadata();
    let metadata = metadata;
    println!("{:?}", metadata);
    let mut cr = xlsx.worksheet_cells_reader("Sheet1")?;
    let (_start, end) = cr.dimensions().end;
    let mut cell_cache: Option<Cell<DataRef>> = None;
    let iter = from_fn(move || {
        let mut row = Vec::with_capacity((end + 1) as usize);
        let mut next_col: u32;
        let target_row;

        // 先处理缓存，或读取第一个 cell 确定本行目标行号
        let first = match cell_cache.take() {
            Some(c) => c,
            None => match cr.next_cell().ok()? {
                Some(c) => c,
                None => return None,
            },
        };

        target_row = first.get_position().0;
        for _ in 0..first.get_position().1 {
            row.push(DataRef::Empty);
        }
        row.push(first.get_value().clone());
        next_col = first.get_position().1 + 1;

        while let Some(cell) = cr.next_cell().ok()? {
            if cell.get_position().0 != target_row {
                cell_cache = Some(cell);
                break;
            }
            while next_col < cell.get_position().1 {
                row.push(DataRef::Empty);
                next_col += 1;
            }
            row.push(cell.get_value().clone());
            next_col = cell.get_position().1 + 1;
            if next_col > end {
                break;
            }
        }

        while next_col <= end {
            row.push(DataRef::Empty);
            next_col += 1;
        }

        if row.is_empty() { None } else { Some(row) }
    });

    let mut current_row = 0;
    for row in iter {
        println!("{:?}\n", row);
        current_row += 1;
        if current_row >= 10 {
            break;
        }
    }

    Ok(())
}

fn cell_to_any_value<'a>(cell: &'a Cell<DataRef<'a>>) -> AnyValue<'a> {
    match cell.get_value() {
        DataRef::Int(v) => AnyValue::Int64(*v),
        DataRef::Float(v) => AnyValue::Float64(*v),
        DataRef::Bool(v) => AnyValue::Boolean(*v),
        DataRef::String(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
        DataRef::SharedString(v) => AnyValue::String(v),
        DataRef::DateTime(v) => excel_datetime_to_any(v),
        DataRef::DateTimeIso(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
        DataRef::DurationIso(v) => AnyValue::StringOwned(PlSmallStr::from(v.as_str())),
        DataRef::Error(_) => AnyValue::Null,
        DataRef::Empty => AnyValue::Null,
    }
}

/// Excel serial datetime -> Polars Datetime (Nanoseconds)
fn excel_datetime_to_any(dt: &ExcelDateTime) -> AnyValue<'static> {
    let (y, m, d, h, min, s, milli) = dt.to_ymd_hms_milli();
    let naive = NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
        .and_then(|date| date.and_hms_milli_opt(h as u32, min as u32, s as u32, milli as u32))
        .unwrap_or_else(|| NaiveDateTime::MIN);

    let nanos = naive.and_utc().timestamp_nanos_opt().unwrap_or(0);
    AnyValue::Datetime(nanos, TimeUnit::Nanoseconds, None)
}
