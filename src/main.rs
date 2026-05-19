use clap::{CommandFactory, Parser};
use project_x::*;

fn main() {
    let args = Args::parse();
    match args.pattern {
        Pattern::Csv {
            ref path,
            ref output,
            ref sheet_name,
            ref sheet_idx,
        } => {
            if let Err(e) = csv_save(&args, path, output, sheet_name, sheet_idx) {
                eprintln!("csv 保存失败: {}", e);
                std::process::exit(1);
            }
        }
        Pattern::Test {
            ref parttern,
            ref path,
            ref rows,
            ref col,
            ref no_limit,
        } => test_parttern(&args, path, parttern, *rows, *col, *no_limit),
        Pattern::Completion { shell } => {
            let mut cmd = Args::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
    }
}
