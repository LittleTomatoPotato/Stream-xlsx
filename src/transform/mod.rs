use crate::Args;
use clap::ValueEnum;

use polars::prelude::*;
use stream_xlsx::df_iter::df_iter;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
pub enum Format {
    /// 流式转化为csv
    Csv,
    /// 流式转化为parquet
    Parquet,
}

fn get_iter(
    args: &Args,
    path: &std::path::PathBuf,
    sheet_name: &Option<String>,
    sheet_idx: &usize,
) -> anyhow::Result<Box<dyn Iterator<Item = anyhow::Result<polars::prelude::DataFrame>>>> {
    let iter: Box<dyn Iterator<Item = anyhow::Result<polars::prelude::DataFrame>>> =
        Box::new(df_iter(
            args.batch_size,
            path,
            sheet_name.as_deref(),
            Some(*sheet_idx),
            true,
        )?);
    Ok(iter)
}

pub fn csv_save(
    args: &Args,
    path: &std::path::PathBuf,
    output: &Option<std::path::PathBuf>,
    sheet_name: &Option<String>,
    sheet_idx: &usize,
) -> anyhow::Result<()> {
    let iter = get_iter(args, path, sheet_name, sheet_idx)?;
    let outputfile = match output {
        &Some(ref file) => file.clone().with_extension("csv"),
        None => path.with_extension("csv"),
    };
    let mut file = std::fs::File::create(&outputfile)?;
    let mut is_first = true;

    for df in iter {
        let mut df = df?;
        let mut writer = CsvWriter::new(&mut file);
        if !is_first {
            writer = writer.include_header(false);
        }
        writer.finish(&mut df)?;
        is_first = false;
    }

    Ok(())
}

pub fn save_to_parquet(
    args: &Args,
    path: &std::path::PathBuf,
    output: &Option<std::path::PathBuf>,
    sheet_name: &Option<String>,
    sheet_idx: &usize,
) -> anyhow::Result<()> {
    let mut iter = get_iter(args, path, sheet_name, sheet_idx)?;
    let outputfile = match output {
        &Some(ref file) => file.clone().with_extension("parquet"),
        None => path.with_extension("parquet"),
    };
    let file = std::fs::File::create(&outputfile)?;

    let first_df = match iter.next() {
        Some(res) => res?,
        None => {
            println!("第一个df为空");
            return Ok(());
        }
    };

    let mut writer = ParquetWriter::new(file)
        .with_row_group_size(args.batch_size)
        .batched(first_df.schema())?;

    writer.write_batch(&first_df)?;

    for df in iter {
        let df = df?;
        writer.write_batch(&df)?;
    }

    writer.finish()?;
    Ok(())
}
