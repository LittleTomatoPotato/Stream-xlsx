use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rust_xlsxwriter::{ExcelDateTime, Workbook};

pub fn generate(
    path: &std::path::PathBuf,
    rows: usize,
    col: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = path.as_path();

    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    let addition_cols = col.saturating_sub(7).max(0);

    // 写入表头：对应 calamine 常见的 DataType 类别
    let headers = [
        "Int",
        "Float",
        "String",
        "Bool",
        "DateTime",
        "DurationIso",
        "Empty",
    ];
    for (col, header) in headers.iter().enumerate() {
        worksheet.write(0, col as u16, *header)?;
    }

    let mut rng = StdRng::seed_from_u64(42);

    for row in 1..=rows {
        let r = row as u32;

        // 1. Int
        let int_val: i64 = rng.random_range(-1_000_000..1_000_000);
        worksheet.write(r, 0, int_val)?;

        // 2. Float
        let float_val: f64 = rng.random_range(-1e6..1e6);
        worksheet.write(r, 1, float_val)?;

        // 3. String
        let str_val = format!("row_{}_rnd{}", row, rng.random::<u32>());
        worksheet.write(r, 2, &str_val)?;

        // 4. Bool
        let bool_val: bool = rng.random();
        worksheet.write(r, 3, bool_val)?;

        // 5. DateTime (Excel 原生日期时间)
        let year = rng.random_range(2000..2025);
        let month = rng.random_range(1..13);
        let day = rng.random_range(1..28);
        let hour = rng.random_range(0..24);
        let minute = rng.random_range(0..60);
        let second = rng.random_range(0..60);
        let dt_str = format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            year, month, day, hour, minute, second
        );
        let dt = ExcelDateTime::parse_from_str(&dt_str)?;
        worksheet.write_datetime(r, 4, &dt)?;

        // 6. DurationIso (以字符串形式存储，calamine 会识别为 DurationIso)
        let h = rng.random_range(0..100);
        let m = rng.random_range(0..60);
        let s = rng.random_range(0..60);
        let dur_iso = format!("PT{}H{}M{}S", h, m, s);
        worksheet.write(r, 5, &dur_iso)?;

        // 7. Empty：50% 概率留空，50% 概率写入占位符
        if rng.random_bool(0.5) {
            worksheet.write(r, 6, "filled")?;
        }
        // 否则该单元格保持未写入状态，即为 Empty

        // 写入额外字符串列
        for i in 0..addition_cols {
            let str_val = format!("row_{}_rnd{}", row, rng.random::<u32>());
            worksheet.write(r, (7 + i) as u16, &str_val)?;
        }
    }

    workbook.save(path)?;
    println!(
        "Successfully generated '{}' with {} rows × {} columns.",
        path.display(),
        rows,
        headers.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_test() {
        let n = 1;
        for i in 0..n {
            println!("{}", i)
        }
    }
}
