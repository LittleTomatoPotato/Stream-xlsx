use crate::{Args, df_iter, generate_test_xlsx};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
pub enum TestMod {
    /// 可以print DataFrame, 只打印前10个, 页打印加载时间
    Debug,
    /// 可以返回实际的DataFrame总批次数,以及加载时间
    Count,
    /// 生成测试文件
    TestFile,
}

fn run_count<I>(start: std::time::Instant, df_iter: I)
where
    I: Iterator<Item = anyhow::Result<polars::prelude::DataFrame>>,
{
    let mut count: usize = 0;
    df_iter.for_each(|df| {
        if df.is_ok() {
            count += 1;
        }
    });
    let elapsed = start.elapsed();
    println!("{} {:?}", count, elapsed);
}

fn run_debug<I>(start: std::time::Instant, df_iter: I, no_limit: bool)
where
    I: Iterator<Item = anyhow::Result<polars::prelude::DataFrame>> + ExactSizeIterator,
{
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
        TestMod::TestFile => {
            match generate_test_xlsx::generate(path, rows, col) {
                Ok(_) => {
                    println!("测试文件成功生成: {:?}", path)
                }
                Err(e) => {
                    println!("测试文件生成失败: {}", e)
                }
            };
            return;
        }
        _ => {}
    }

    let df_iter = match df_iter(args.batch_size, path, None, 0.into(), true, None) {
        Ok(a) => a,
        Err(e) => {
            println!("文件打开错误: {}, 输入路径:{:?}", e, path);
            return;
        }
    };
    match parttern {
        TestMod::Count => run_count(start, df_iter),
        TestMod::Debug => run_debug(start, df_iter, no_limit),
        _ => unreachable!(),
    }
}
