pub mod generate_test_xlsx;
pub mod shell_completion;
pub mod transform;
pub use transform::*;
pub mod testmode;
pub use testmode::*;

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
    pub batch_size: Option<usize>,
    #[arg(short = 'i', long, global = true)]
    pub ignore_case: bool,
    #[arg(short, long, global = true)]
    pub ext: Option<String>,
    /// (保留占位，reader 参数已移除)
    #[arg(
        short = 'R',
        long = "reader",
        default_value = "lm",
        global = true,
        hide = true
    )]
    pub reader: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Pattern {
    /// 流式转化为其他文件格式(csv、parquet)
    Tf {
        // Transform
        #[arg(value_enum)]
        format: Format,
        path: std::path::PathBuf,
        #[arg(default_value=None,short='N',long)]
        sheet_name: Option<String>,
        #[arg(default_value = "0", short = 'I', long)]
        sheet_idx: usize,
        #[arg(default_value = None)]
        output: Option<std::path::PathBuf>,
    },
    /// 测试: 可生成测试xlsx文件、测试读取速度
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
    /// 安装 shell 自动补全脚本
    Completion {
        /// 指定 shell（不指定则自动检测）
        #[arg(value_enum)]
        shell: Option<clap_complete::Shell>,
    },
}
