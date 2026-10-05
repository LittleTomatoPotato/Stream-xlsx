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

Pass an ordered Polars schema to enable strict streaming reads. Header order may
change in the workbook; yielded frames always follow schema order:

```python
import polars as pl
import stream_xlsx_py as sx

reader = sx.read_xlsx(
    "data.xlsx",
    batch_size=10_000,
    fast=True,
    schema={
        "id": pl.Int64,
        "amount": pl.Float64,
        "created_at": pl.Datetime("us"),
    },
)
for frame in reader:
    print(frame.schema)
```

Strict mode remains batch-streaming. It does not infer or widen types except for
lossless integer input into a declared `Float64`; a mismatch raises an exception
at the batch containing the offending cell and stops the iterator.

The reader also supports selecting worksheets, skipping rows, and reading
files with or without a header row. See the
[project README](https://github.com/LittleTomatoPotato/Stream-xlsx#readme) for
benchmarks, limitations, and the complete API overview.
