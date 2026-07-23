# stream-xlsx-py

`stream-xlsx-py` is the Python binding for
[`stream_xlsx`](https://github.com/LittleTomatoPotato/Stream-xlsx), a streaming
XLSX reader that yields Polars DataFrames in batches.

## Installation

```bash
pip install stream-xlsx-py
```

## Usage

```python
import stream_xlsx_py as sx

reader = sx.read_xlsx("data.xlsx", batch_size=10_000)
for frame in reader:
    print(frame.shape)
```

Enable concurrent parsing with fast mode:

```python
reader = sx.read_xlsx(
    "data.xlsx",
    batch_size=10_000,
    fast=True,
    fast_parallelism=8,
)
```

The reader also supports selecting worksheets, skipping rows, and reading
files with or without a header row. See the
[project README](https://github.com/LittleTomatoPotato/Stream-xlsx#readme) for
benchmarks, limitations, and the complete API overview.
