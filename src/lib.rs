pub mod generate_test_xlsx;
use std::fmt::Display;

use clap::{Parser, Subcommand, ValueEnum};
pub use stream_xlsx;
use stream_xlsx::df_iter::df_iter;

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub pattern: Pattern,
    #[arg(
        short = 'B',
        long = "batchsize",
        default_value = "10000",
        global = true
    )]
    pub batch_size: usize,
    #[arg(short = 'i', long, global = true)]
    pub ignore_case: bool,
    #[arg(short, long, global = true)]
    pub ext: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Pattern {
    Csv {
        path: std::path::PathBuf,
        #[arg(default_value=None,short='N',long)]
        sheet_name: Option<String>,
        #[arg(default_value = "0", short = 'I', long)]
        sheet_idx: usize,
        #[arg(default_value = None)]
        output: Option<std::path::PathBuf>,
    },
    Test {
        #[arg(value_enum)]
        parttern: TestMod,
        #[arg(default_value = "test_data.xlsx")]
        path: std::path::PathBuf,
        #[arg(default_value = "100000", short = 'r', long)]
        rows: usize,
        #[arg(default_value = "7", short = 'c', long)]
        col: usize,
        #[arg(short = 'l', long)]
        no_limit: bool,
    },
    /// 生成 shell 自动补全脚本
    Completion {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
#[non_exhaustive]
pub enum TestMod {
    Debug,
    Count,
    TestFile,
}
impl Display for TestMod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Count => "count",
            Self::Debug => "debug",
            Self::TestFile => "testfile",
        };
        value.fmt(f)
    }
}

pub fn test_parttern(
    args: &Args,
    path: &std::path::PathBuf,
    parttern: &TestMod,
    rows: usize,
    col: usize,
    no_limit: bool,
) {
    let start = std::time::Instant::now();

    match parttern {
        TestMod::Count => {
            let df_iter =
                match stream_xlsx::df_iter::df_iter(args.batch_size, path, None, 0.into(), true) {
                    Ok(a) => a,
                    Err(e) => {
                        println!("文件打开错误: {}, 输入路径:{:?}", e, path);
                        return;
                    }
                };
            let mut count: usize = 1;
            df_iter.for_each(|df| {
                if df.is_ok() {
                    count += 1;
                }
            });
            let elapsed = start.elapsed();
            println!("{} {:?}", count, elapsed);
        }
        TestMod::Debug => {
            let df_iter =
                match stream_xlsx::df_iter::df_iter(args.batch_size, path, None, 0.into(), true) {
                    Ok(a) => a,
                    Err(e) => {
                        println!("文件打开错误: {}, 输入路径:{:?}", e, path);
                        return;
                    }
                };
            let mut count: usize = 1;
            let total_df_num = df_iter.len();
            if !no_limit {
                df_iter.take(10).for_each(|df| match df {
                    Ok(df) => {
                        println!("Batch {}: {}", count, df);
                        count += 1;
                    }
                    Err(e) => {
                        eprintln!("Batch {} error: {}", count, e);
                        count += 1;
                    }
                });
            } else {
                df_iter.for_each(|df| match df {
                    Ok(df) => {
                        if count < 10 {
                            println!("Batch {}: {}", count, df);
                        }
                        count += 1;
                    }
                    Err(e) => {
                        eprintln!("Batch {} error: {}", count, e);
                        count += 1;
                    }
                });
            }

            let elapsed = start.elapsed();
            println!(
                "Total df :{}. Debug mode show up to 10\nTotal batches: {}, elapsed: {:?}",
                total_df_num,
                count - 1,
                elapsed
            );
        }
        TestMod::TestFile => {
            match generate_test_xlsx::generate(path, rows, col) {
                Ok(_) => {
                    println!("测试文件成功生成: {:?}", path)
                }
                Err(e) => {
                    println!("测试文件生成失败: {}", e)
                }
            };
        }
    }
}

pub fn csv_save(
    args: &Args,
    path: &std::path::PathBuf,
    output: &Option<std::path::PathBuf>,
    sheet_name: &Option<String>,
    sheet_idx: &usize,
) -> anyhow::Result<()> {
    let iter = df_iter(
        args.batch_size,
        path,
        sheet_name.as_deref(),
        Some(*sheet_idx),
        true,
    )?;
    let outputfile = match output {
        &Some(ref file) => file.clone().with_extension("csv"),
        None => path.with_extension("csv"),
    };
    use polars::prelude::*;
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
