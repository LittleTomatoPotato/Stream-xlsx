from typing import Iterator, Optional

import polars as pl

class XlsxReader(Iterator[pl.DataFrame]):
    """流式 xlsx 读取器，惰性逐批产生 DataFrame。支持多 sheet 切换。

    用法::

        reader = stream_xlsx_py.read_xlsx("data.xlsx")
        for df in reader:
            print(df.shape)

        # 切换 sheet
        reader.select_sheet("Sheet2")
        for df in reader:
            print(df.shape)
    """

    def __iter__(self) -> "XlsxReader": ...
    def __next__(self) -> pl.DataFrame: ...
    def __len__(self) -> int: ...
    def sheet_names(self) -> list[str]:
        """返回所有 sheet 名称列表。"""
        ...
    def select_sheet(self, sheet_name: str) -> None:
        """按名称切换 sheet，重置迭代器状态。"""
        ...
    def select_sheet_by_idx(self, sheet_idx: int) -> None:
        """按索引切换 sheet（0-based），重置迭代器状态。"""
        ...

def read_xlsx(
    path: str,
    batch_size: Optional[int] = 10000,
    sheet_name: Optional[str] = None,
    sheet_idx: Optional[int] = None,
    has_header: bool = True,
    skip_rows: Optional[list[int]] = None,
    fast: bool = False,
    fast_parallelism: Optional[int] = None,
) -> XlsxReader:
    """打开 xlsx 文件，返回惰性迭代器。

    参数:
        path: 文件路径。
        batch_size: 每批读取的行数，默认 10000。传 None 时一次性产出全部行。
        sheet_name: 工作表名称（可选）。
        sheet_idx: 工作表索引，从 0 开始（可选，与 sheet_name 互斥）。
        has_header: 是否将第一行作为表头，默认 True。
        skip_rows: 需要跳过的 0-based 行索引列表（可选，不影响 header 解析）。
        fast: 是否使用 fast 并发解析模式（~3x 加速，多 ~30% 内存），默认 False。
        fast_parallelism: fast 模式 worker 线程数（可选，默认自动；若超过机器核心数会自动减 2）。

    返回:
        XlsxReader: 可迭代的 DataFrame 生成器，支持多 sheet 切换。

    示例::

        # 默认模式（低内存）
        reader = stream_xlsx_py.read_xlsx("data.xlsx", batch_size=10000)

        # fast 模式（~3x 速度）
        reader = stream_xlsx_py.read_xlsx("data.xlsx", batch_size=10000, fast=True)

        # 自定义 worker 线程数
        reader = stream_xlsx_py.read_xlsx(
            "data.xlsx", fast=True, fast_parallelism=4
        )
    """
    ...
