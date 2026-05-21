# stream_xlsx

流式 xlsx 读取器，支持 Rust 库、CLI 工具和 Python 绑定。基于 quick-xml + zip 实现真正的流式解析，**不一次性将整张表载入内存**。

## 项目结构

```
project_x/          # CLI 工具（cargo build）
stream_xlsx/        # 纯 Rust 库（rlib）
stream_xlsx_py/     # pyo3 Python 绑定（maturin build）
```

## 特点

- **流式读取**：逐 batch 产出 Polars DataFrame，100 万行 × 60 列（659 MB）只需 ~18–21 秒
- **双模式 Reader**：`default`（原始实现）与 `lm`（低内存优化），CLI 通过 `--reader` 切换，Python 通过 `reader=` 参数切换
- **低内存**：`lm` 模式峰值内存比 `default` 降低 **~35–50%**，比 polars+calamine 降低 **~70%**
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
pip install target/wheels/stream_xlsx_py-*.whl
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

# 使用低内存模式（推荐大文件）
project_x --reader lm test count data.xlsx

# 生成测试文件：100 万行 × 60 列
project_x test test-file big.xlsx --rows 1000000 --col 60

# Shell 自动补全
project_x completion zsh > ~/.zsh_completions/_project_x
```

### Python

```python
import stream_xlsx_py as sx

# 默认 reader
for df in sx.read_xlsx("data.xlsx", batch_size=10000):
    print(df.shape)

# 低内存模式（推荐大文件）
for df in sx.read_xlsx("data.xlsx", batch_size=10000, reader="lm"):
    print(df.shape)
```

## Benchmark

测试文件：`test_100w_60c.xlsx`（100 万行 × 60 列，`project_x test test-file --rows 1000000 --col 60` 生成，约 659 MB）

测试环境：macOS, Apple Silicon, release 构建。内存采样间隔 50 ms。

### Python 环境对比

在 Python 进程中对比 `stream_xlsx_py` 流式遍历与 `polars.read_excel` 全量加载：

| 方案 | 时间 | 峰值内存 | 说明 |
|------|------|---------|------|
| **stream_xlsx_py (lm, bs=10k)** | **19.87 s** | **2,619 MB** | 流式遍历，低内存模式 |
| stream_xlsx_py (default, bs=10k) | 19.55 s | 4,960 MB | 流式遍历，默认模式 |
| stream_xlsx_py (lm, bs=1M) | 20.24 s | 6,120 MB | 全量加载（单 batch） |
| stream_xlsx_py (default, bs=1M) | 19.37 s | 6,530 MB | 全量加载（单 batch） |
| polars + calamine | 25.05 s | 8,858 MB | `pl.read_excel(engine="calamine")` |
| polars + xlsx2csv | 95.16 s | 8,526 MB | `pl.read_excel(engine="xlsx2csv")` |

**结论**：
- `stream_xlsx_py` 比 polars+calamine **快 21%**，内存 **低 70%**
- `lm` 模式比 `default` 内存再降 **47%**，时间持平
- 全量加载（bs=1M）时 stream_xlsx_py 与 polars 内存接近，但仍快 **23%**

![Python 环境内存曲线](docs/benchmark/python_memory_comparison.png)

上图可见 polars calamine 在 ~25 s 达到 ~8.9 GB 峰值，而 stream_xlsx_py 全程稳定在 2.6–6.5 GB；`lm` 模式上升最平缓，峰值最低。

### CLI 环境对比（default vs lm）

逐 batch 遍历不保留中间结果（`project_x test count`）：

| batch_size | default 时间 | lm 时间 | default 峰值 | lm 峰值 | 降幅 |
|-----------|-------------|---------|-------------|---------|------|
| 1,000 | 18.87 s | **18.03 s** | 4,949 MB | **2,512 MB** | **–49%** |
| 5,000 | 18.65 s | **18.00 s** | 4,943 MB | **2,525 MB** | **–49%** |
| **10,000** | 18.62 s | **18.11 s** | 4,941 MB | **2,546 MB** | **–48%** |
| 50,000 | 19.89 s | **18.04 s** | 4,948 MB | **2,750 MB** | **–44%** |
| 100,000 | 18.97 s | **18.04 s** | 4,955 MB | **3,183 MB** | **–36%** |
| 1,000,000 | 18.68 s | **18.42 s** | 6,429 MB | **6,074 MB** | **–6%** |

**结论**：
- `lm` 比 `default` **快 3–5%**，内存降低 **36–49%**（流式场景）
- batch_size 对时间影响极小，瓶颈在 XML 解析和 ZIP 解压
- **全量加载（bs=1M）**：lm 优势收窄（–6%），因为最终需要容纳 100 万行 × 60 列的 DataFrame，此时内存主要由 Polars 决定

![CLI 全条件内存网格](docs/benchmark/cli_memory_all_grid.png)

上图展示了 12 个测试条件的内存曲线网格（2 readers × 6 batch sizes）。default（上行）在 ~7 s 出现明显峰值后骤降，lm（下行）曲线更平滑，全程线性上升后直接进入平稳期。全量加载（bs=1M，最右列）两者内存趋于一致，因为最终都需要容纳完整的 DataFrame。

![CLI batch=10000 对比](docs/benchmark/cli_memory_batch_10000.png)

batch=10000 时对比最清晰：default 在 7 s 达到 ~5 GB 峰值后骤降，lm 线性上升至 ~2.5 GB 后平稳，无 spikes。

### 为什么 lm 内存更低？

default 的 `prepare()` 阶段使用 `read_to_end` 一次性读取 `sharedStrings.xml`（约 2 GB），ZIP 解压缓冲 + XML 解析器缓冲共同推高内存到 ~5 GB；解析完成后缓冲释放，造成 50 ms 内骤降 ~2 GB。

lm 的 `prepare()` 改为 `BufReader` 直接流式解析，无中间 `Vec<u8>` 缓冲；`sharedStrings` 边读边解析到 `Vec<Box<str>>`，每元素比 `Vec<String>` 省 24 bytes 容量开销。内存曲线呈线性增长后直接进入平稳期。

### 推荐配置

| 场景 | 推荐参数 |
|------|---------|
| 超大文件（>100 MB）| `--reader lm --batchsize 10000` |
| 中等文件（10–100 MB）| `--reader lm --batchsize 10000` |
| 小文件（<10 MB）| `--batchsize 10000`（default 即可） |
| 全量加载到单个 DataFrame | `--batchsize 1000000`（bs=1M 等效全量） |

小文件示例：

```bash
$ project_x --batchsize 10000 test count test_data.xlsx
11 145ms
```

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
