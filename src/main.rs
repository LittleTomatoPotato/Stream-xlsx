use clap::Parser;
use sxlsx::*;

fn main() {
    let args = Args::parse();
    match args.pattern {
        Pattern::Tf {
            ref format,
            ref path,
            ref output,
            ref sheet_name,
            ref sheet_idx,
        } => match format {
            Format::Csv => {
                if let Err(e) = csv_save(&args, path, output, sheet_name, sheet_idx) {
                    eprintln!("csv 保存失败: {}", e);
                    std::process::exit(1);
                }
            }
            Format::Parquet => {
                if let Err(e) = save_to_parquet(&args, path, output, sheet_name, sheet_idx) {
                    eprintln!("csv 保存失败: {}", e);
                    std::process::exit(1);
                }
            }
        },
        Pattern::Test {
            ref parttern,
            ref path,
            ref rows,
            ref col,
            ref no_limit,
        } => test_parttern(&args, path, parttern, *rows, *col, *no_limit),
        Pattern::Completion { shell } => {
            if let Err(e) = sxlsx::shell_completion::install(shell) {
                eprintln!("补全安装失败: {}", e);
                std::process::exit(1);
            }
        }
    }
}
