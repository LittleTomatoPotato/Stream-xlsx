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
| 1,000,000 | 19.37 s | — |

**观察**：
- 流式遍历比全量加载更快（18 s vs 19 s），因为无需在内存中维持超大 DataFrame
- **batch_size=10,000 是甜点**，过小（1k）会增加 Python 迭代开销，过大（100k）使每批 Polars 构建成本上升
- 10k 行/批在速度和内存之间取得了最佳平衡

小文件（10 万行 × 7 列）同样流畅：

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
