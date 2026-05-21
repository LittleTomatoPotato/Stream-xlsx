# stream_xlsx

流式 xlsx 读取器，支持 Rust 库、CLI 工具和 Python 绑定。基于 quick-xml + zip 实现真正的流式解析，**不一次性将整张表载入内存**。

## 项目结构

```
project_x/          # CLI 工具（cargo build）
stream_xlsx/        # 纯 Rust 库（rlib）
stream_xlsx_py/     # pyo3 Python 绑定（maturin build）
```

## 特点

- **流式读取**：逐 batch 产出 Polars DataFrame，100 万行 × 60 列（659 MB）也只需 ~18 秒
- **低内存**：峰值内存与文件大小解耦，batch_size 可控
- **类型推断**：边读边推断列类型（Int → Float → String），空值不参与推断
- **日期支持**：读取 `xl/styles.xml` 的 `cellXfs` + 自定义 `numFmt`，自动识别日期列
- **Shell 补全**：内置 zsh / bash 自动补全生成

## 安装

### CLI

```bash
cargo build --release
# 二进制位于 target/release/project_x
```

### Python

```bash
cd stream_xlsx_py
maturin build --release
pip install ../target/wheels/stream_xlsx_py-*.whl
```

## 使用

### CLI

```bash
# 导出为 CSV
project_x csv data.xlsx --output out.csv

# 指定 sheet（按名称或索引）
project_x csv data.xlsx --sheet-name "Sheet1"
project_x csv data.xlsx --sheet-idx 0

# 统计行数（性能基准）
project_x test count data.xlsx

# 生成测试文件：100 万行 × 60 列
project_x test test-file big.xlsx --rows 1000000 --col 60

# Shell 自动补全
project_x completion zsh > ~/.zsh_completions/_project_x
```

### Python

```python
import stream_xlsx_py as stream_xlsx

# 惰性迭代，每批 10,000 行
reader = stream_xlsx.read_xlsx("data.xlsx", batch_size=10000)
for df in reader:
    print(df.shape)   # (10000, 60)
    # df 是 polars.DataFrame，直接处理
```

## 性能对比

测试文件：**100 万行 × 60 列**（`project_x test test-file --rows 1000000 --col 60` 生成，约 659 MB）

### 1. 全量加载对比

将整张表一次性读入单个 DataFrame：

| 方案 | 耗时 | 说明 |
|------|------|------|
| **stream_xlsx_py (bs=1M)** | **19.37 s** | 单 batch，等效全量 |
| polars + calamine | 24.30 s | `pl.read_excel(engine="calamine")` |
| polars + xlsx2csv | 93.80 s | `pl.read_excel(engine="xlsx2csv")` |
| polars + openpyxl | >60 s | 超时未完成 |

> 测试环境：macOS, Apple Silicon, release 构建。stream_xlsx 比 polars+calamine 快约 **20%**。

### 2. 流式读取对比（stream_xlsx 不同 batch_size）

逐 batch 遍历，不保留中间结果（`for df in reader: pass`）：

| batch_size | Python 绑定 | CLI count |
|-----------|-------------|-----------|
| 1,000 | 18.96 s | 18.29 s |
| 5,000 | 18.76 s | 18.27 s |
| **10,000** | **18.57 s** | **18.47 s** |
| 50,000 | 18.70 s | 18.41 s |
| 100,000 | 18.77 s | 18.46 s |
| 1,000,000 | 19.37 s | 18.61  |

**观察**：
- 流式遍历比全量加载更快（18 s vs 19 s），因为无需在内存中维持超大 DataFrame
- 当前问价大小下**batch_size=10,000 是甜点**，过小（1k）会增加 Python 迭代开销，过大（100k）使每批 Polars 构建成本上升
- 10k 行/批在速度和内存之间取得了最佳平衡

小文件（10 万行 × 7 列）同样流畅：

```bash
$ project_x --batchsize 10000 test count test_data.xlsx
11 145ms
```

## 内存使用情况

测试文件：`test_100w_60c.xlsx`（100 万行 × 60 列，约 659 MB）

文件内部结构：
- `xl/sharedStrings.xml`：约 **2.0 GB**（几乎未压缩）
- `xl/worksheets/sheet1.xml`：约 **2.3 GB**（几乎未压缩）

### 内存时间线（batch_size=10,000）

实时采样（50 ms 间隔）观察到非常明显的**两阶段内存模式**：

| 时间 | 内存 | 阶段 |
|------|------|------|
| 0.00 s | 0.5 MB | 进程启动 |
| 1.0 s | ~1.2 GB | 开始解压/解析 `sharedStrings.xml` |
| 6.0 s | ~4.4 GB | sharedStrings 持续膨胀 |
| **7.02 s** | **4,954.6 MB** | **峰值** |
| **7.07 s** | **2,918.3 MB** | **骤降 ~2,036 MB** |
| 8.5 s ~ 18.5 s | ~2,956 MB | **流式读取阶段，内存几乎持平** |

```
内存 (GB)
   5.0 ┤        ╭─╮
   4.5 ┤      ╭─╯ │     prepare()：解析 sharedStrings.xml
   4.0 ┤    ╭─╯   │     （ZIP 解压缓冲 + XML 解析器缓冲持续累积）
   3.5 ┤  ╭─╯     │
   3.0 ┤─╯        ╰──────────── 流式读取：sheet1.xml
   2.5 ┤                         （内存稳定，batch 间几乎无累积）
       └────┬────┬────┬────┬────┬────
            0    2    4    6    8   10  时间 (s)
```

**骤降原因**：`prepare()` 阶段一次性读取 `sharedStrings.xml`（约 2 GB）时，ZIP 解压缓冲和 XML 解析器缓冲共同推高了内存；一旦 sharedStrings 解析完成并转为 `Vec<String>` 常驻，解压缓冲立即释放，导致内存**在 50 ms 内骤降约 2 GB**。

### 不同 batch_size 的峰值内存

| batch_size | 峰值内存 | 说明 |
|-----------|---------|------|
| 1,000 | **5.20 GB** | 同上，prepare 阶段峰值 |
| 10,000 | **5.47 GB** | Polars 构建 DataFrame 时临时分配略高 |
| 100,000 | **5.20 GB** | 与 1k 持平 |
| 1,000,000 | **6.80 GB** | 单 batch 容纳 100 万行 DataFrame，额外 **+1.6 GB** |

### 分析

- **基线 ~5.2 GB** 主要由三部分构成：
  1. `sharedStrings.xml` 解析后的 `Vec<String>` 常驻内存（约 2 GB）
  2. ZIP 解压缓冲 + quick-xml 解析器缓冲（约 2 GB+）
  3. 每批 Polars DataFrame 的临时分配（几百 MB）

- **batch_size 对内存影响有限**：1k 与 100k 的峰值内存几乎相同，因为瓶颈不在 DataFrame 本身，而在 sharedStrings 解析阶段的缓冲。

- **流式读取阶段内存几乎持平**：一旦进入 sheet 数据读取，每批 DataFrame 在迭代后被 drop，内存曲线呈水平线，**无累积增长**。

- **全量加载（bs=1M）内存显著上升**：从 5.2 GB 增至 6.8 GB，增量约 1.6 GB，即 100 万行 × 60 列 DataFrame 的内存 footprint。

> 结论：对于超大 xlsx，流式读取的核心价值有两层：
> 1. **避免 prepare 阶段以外的内存 spikes**（sheet 数据读取时内存稳定）
> 2. **避免一次性构建超大 DataFrame**（bs=1M 比 bs=10k 多占 1.6 GB）

## 构建

### 开发构建

```bash
# Rust 库 + CLI
cargo build

# 测试
cargo test --workspace

# Python wheel
cd stream_xlsx_py
maturin develop   # 开发模式，直接链接到 .venv
maturin build --release
```

### CI

项目已配置 Gitea Actions（`.gitea/workflows/ci.yaml`），每次 push/PR 自动运行：

1. `cargo test --workspace`
2. `cargo build --release`（CLI artifact）
3. `maturin build --release`（wheel artifact）
