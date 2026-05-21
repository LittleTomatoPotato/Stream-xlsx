from typing import Iterator, Optional

import polars as pl

class XlsxReader(Iterator[pl.DataFrame]):
    """流式 xlsx 读取器，惰性逐批产生 DataFrame。"""

    def __iter__(self) -> "XlsxReader": ...
    def __next__(self) -> pl.DataFrame: ...
    def __len__(self) -> int: ...

def read_xlsx(
    path: str,
    batch_size: int = 10000,
    sheet_name: Optional[str] = None,
    sheet_idx: Optional[int] = None,
    has_header: bool = True,
    reader: str = "default",
) -> XlsxReader:
    """打开 xlsx 文件，返回惰性迭代器。

    参数:
        path: 文件路径。
        batch_size: 每批读取的行数，默认 10000。
        sheet_name: 工作表名称（可选）。
        sheet_idx: 工作表索引，从 0 开始（可选）。
        has_header: 是否将第一行作为表头，默认 True。
        reader: 读取器类型，"default" 或 "lm"，默认 "default"。

    返回:
        XlsxReader: 可迭代的 DataFrame 生成器。
    """
    ...
