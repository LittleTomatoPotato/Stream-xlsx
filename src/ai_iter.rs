use calamine::{DataRef, Xlsx, XlsxError};
use std::io::{Read, Seek};

/// 惰性逐行迭代器：每次只保留一行在内存里
pub fn lazy_rows<'a, RS: Read + Seek>(
    xlsx: &'a mut Xlsx<RS>,
    sheet_name: &str,
) -> Result<impl Iterator<Item = Result<Vec<calamine::Cell<DataRef<'a>>>, XlsxError>> + 'a, XlsxError>
{
    let mut reader = xlsx.worksheet_cells_reader(sheet_name)?;
    let mut staging: Option<calamine::Cell<DataRef<'a>>> = None;

    Ok(std::iter::from_fn(move || {
        let mut row = Vec::new();

        // 把上一行缓存下来的第一个单元格放回当前行
        if let Some(cell) = staging.take() {
            row.push(cell);
        }

        loop {
            match reader.next_cell() {
                Ok(Some(cell)) => {
                    let row_idx = cell.get_position().0;
                    if row.is_empty() {
                        row.push(cell);
                    } else if row[0].get_position().0 == row_idx {
                        row.push(cell);
                    } else {
                        // 换行了，缓存这个单元格，下次再处理
                        staging = Some(cell);
                        return Some(Ok(row));
                    }
                }
                Ok(None) => {
                    return if row.is_empty() { None } else { Some(Ok(row)) };
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use crate::ai_iter::lazy_rows;
    use calamine::{Xlsx, open_workbook};
    use std::path::Path;
    #[test]
    fn test_ai_iter() -> Result<(), Box<dyn std::error::Error>> {
        let path = Path::new("test_data.xlsx");
        let mut workbook: Xlsx<_> = open_workbook(path)?;

        // 先取维度（此时 worksheet xml 还没展开）
        let reader = workbook.worksheet_cells_reader("Sheet1")?;
        let dim = reader.dimensions();
        drop(reader); // 必须先 drop，释放对 workbook 的借用

        let total_rows = dim.end.0 - dim.start.0 + 1;
        let total_cols = dim.end.1 - dim.start.1 + 1;
        println!(
            "维度: {} 行 × {} 列 ({} 个单元格)",
            total_rows,
            total_cols,
            dim.len()
        );

        // 重新创建 reader，开始惰性逐行读取
        let rows = lazy_rows(&mut workbook, "Sheet1")?;
        for (i, row) in rows.enumerate() {
            let row = row?;
            let row_idx = row.first().map(|c| c.get_position().0).unwrap_or(0);
            println!(
                "批次 {} -> 实际行号 {}，本行 {} 个单元格",
                i,
                row_idx,
                row.len()
            );
            // 在这里处理这一行，内存里永远只有当前一行
        }

        Ok(())
    }
}
