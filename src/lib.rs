pub mod generate_test_xlsx;
pub mod shell_completion;
pub mod transform;
pub use transform::*;
pub mod testmode;
pub use testmode::*;

use clap::{Parser, Subcommand};
pub use stream_xlsx;
use stream_xlsx::df_iter::df_iter;

pub fn build_fast_config(args: &Args) -> stream_xlsx::FastConfig {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let mut cfg = stream_xlsx::FastConfig::default();
    if let Some(v) = args.fast_parallelism {
        cfg.parallelism = if v > cores {
            cores.saturating_sub(2)
        } else {
            v
        };
    }
    if let Some(v) = args.fast_chunk_size {
        cfg.chunk_size = v;
    }
    if let Some(v) = args.fast_queue_cap {
        cfg.queue_cap_mul = v;
    }
    if let Some(v) = args.fast_temp_kb {
        cfg.temp_size = v * 1024;
    }
    if let Some(v) = args.fast_buf_kb {
        cfg.buf_size = v * 1024;
    }
    cfg
}

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
    /// Fast mode: fully decompress sharedStrings.xml then byte-scan.
    /// Trades ~2-4GB extra peak memory for ~1.5x faster init().
    #[arg(long, global = true)]
    pub fast: bool,
    /// Fast mode: parsing worker threads
    #[arg(long, global = true, value_name = "N")]
    pub fast_parallelism: Option<usize>,
    /// Fast mode: cells per chunk
    #[arg(long, global = true, value_name = "N")]
    pub fast_chunk_size: Option<usize>,
    /// Fast mode: queue capacity multiplier (queue = threads * mul + 1)
    #[arg(long, global = true, value_name = "MUL")]
    pub fast_queue_cap: Option<usize>,
    /// Fast mode: read temp buffer size (KB)
    #[arg(long, global = true, value_name = "KB")]
    pub fast_temp_kb: Option<usize>,
    /// Fast mode: BufReader size (KB)
    #[arg(long, global = true, value_name = "KB")]
    pub fast_buf_kb: Option<usize>,
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
