#!/usr/bin/env python3
"""
性能基准测试脚本 —— 复现 README 中的性能对比数据

依赖:
    pip install polars stream_xlsx_py xlsx2csv

用法:
    python benchmark.py [xlsx文件路径]

默认测试文件:
    test_100w_60c.xlsx (100万行×60列, 约659MB)
    若不存在，可先用 CLI 生成:
        ./target/release/project_x test test-file test_100w_60c.xlsx --rows 1000000 --col 60
"""

import os
import subprocess
import sys
import time
from pathlib import Path

# ---------------------------------------------------------------------------
# 配置
# ---------------------------------------------------------------------------
DEFAULT_BIG_FILE = "test_100w_60c.xlsx"
DEFAULT_SMALL_FILE = "test_data.xlsx"
CLI_BIN = "./target/release/project_x"
BATCH_SIZES = [1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000]


# ---------------------------------------------------------------------------
# 工具函数
# ---------------------------------------------------------------------------
def find_test_file(argv):
    if len(argv) > 1:
        p = Path(argv[1])
        if p.exists():
            return str(p)
        print(f"[错误] 指定文件不存在: {p}")
        sys.exit(1)

    for candidate in (DEFAULT_BIG_FILE, DEFAULT_SMALL_FILE):
        if Path(candidate).exists():
            return candidate

    print(f"[错误] 未找到默认测试文件 ({DEFAULT_BIG_FILE} 或 {DEFAULT_SMALL_FILE})")
    print(f"       请先用 CLI 生成大文件:")
    print(f"       {CLI_BIN} test test-file {DEFAULT_BIG_FILE} --rows 1000000 --col 60")
    sys.exit(1)


def fmt_sec(t: float) -> str:
    if t < 1.0:
        return f"{t * 1000:.2f} ms"
    return f"{t:.2f} s"


def timeit(name: str, fn, *args, **kwargs) -> float:
    print(f"  → 运行 {name} ...", end=" ", flush=True)
    t0 = time.perf_counter()
    try:
        fn(*args, **kwargs)
    except Exception as e:
        print(f"失败 ({e})")
        return float("nan")
    elapsed = time.perf_counter() - t0
    print(f"{fmt_sec(elapsed)}")
    return elapsed


# ---------------------------------------------------------------------------
# 测试项
# ---------------------------------------------------------------------------
def bench_stream_py_stream(path: str, batch_size: int):
    """stream_xlsx_py 流式遍历 (for df in reader: pass)"""
    import stream_xlsx_py as stream_xlsx

    reader = stream_xlsx.read_xlsx(path, batch_size=batch_size)
    for _ in reader:
        pass


def bench_stream_py_full(path: str):
    """stream_xlsx_py 全量加载 (batch_size=1M)"""
    bench_stream_py_stream(path, 1_000_000)


def bench_polars_calamine(path: str):
    """polars + calamine 全量加载"""
    import polars as pl

    pl.read_excel(path, engine="calamine")


def bench_polars_xlsx2csv(path: str):
    """polars + xlsx2csv 全量加载"""
    import polars as pl

    pl.read_excel(path, engine="xlsx2csv")


def parse_rust_duration(s: str) -> float:
    """解析 Rust std::time::Duration 的 Debug 输出，如 151.017792ms / 1s 23ms / 12.345s"""
    total = 0.0
    for m in __import__("re").finditer(r"(\d+(?:\.\d+)?)(ns|µs|ms|s)", s):
        val = float(m.group(1))
        unit = m.group(2)
        if unit == "ns":
            total += val * 1e-9
        elif unit == "µs":
            total += val * 1e-6
        elif unit == "ms":
            total += val * 1e-3
        elif unit == "s":
            total += val
    return total


def bench_cli_count(path: str, batch_size: int) -> float:
    """CLI `project_x test count` —— 解析 CLI 内部计时，避免子进程创建开销"""
    cmd = [CLI_BIN, "test", "count", path, "--batchsize", str(batch_size)]
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    # 输出格式: "<count> <duration>"
    parts = result.stdout.strip().split()
    if len(parts) >= 2:
        return parse_rust_duration(parts[-1])
    raise RuntimeError(f"无法解析 CLI 输出: {result.stdout.strip()}")


# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------
def main():
    path = find_test_file(sys.argv)
    size_mb = Path(path).stat().st_size / (1024 * 1024)
    print(f"测试文件: {path} ({size_mb:.1f} MB)\n")

    results = []

    # --- 1. 全量加载对比 ---
    print("=" * 60)
    print("1. 全量加载对比 (一次性读入单个 DataFrame)")
    print("=" * 60)

    try:
        t = timeit("stream_xlsx_py (bs=1M)", bench_stream_py_full, path)
        results.append(("stream_xlsx_py (bs=1M)", t))
    except ImportError:
        print("  → 跳过 stream_xlsx_py (未安装)")

    try:
        t = timeit("polars + calamine", bench_polars_calamine, path)
        results.append(("polars + calamine", t))
    except ImportError:
        print("  → 跳过 polars + calamine (未安装)")

    try:
        t = timeit("polars + xlsx2csv", bench_polars_xlsx2csv, path)
        results.append(("polars + xlsx2csv", t))
    except ImportError:
        print("  → 跳过 polars + xlsx2csv (未安装)")

    print()
    print("-" * 40)
    print(f"{'方案':<28} {'耗时':>10}")
    print("-" * 40)
    for name, t in results:
        print(f"{name:<28} {fmt_sec(t):>10}")
    print("-" * 40)

    # --- 2. 流式读取对比 ---
    print()
    print("=" * 60)
    print("2. 流式读取对比 (逐 batch 遍历，不保留中间结果)")
    print("=" * 60)

    stream_results = []
    cli_results = []

    for bs in BATCH_SIZES:
        if bs == 1_000_000:
            # 1M 已经在全量加载测过，这里跳过或复用结果
            continue

        label = f"{bs:,}"
        try:
            t = timeit(f"stream_xlsx_py bs={label}", bench_stream_py_stream, path, bs)
            stream_results.append((label, t))
        except ImportError:
            stream_results.append((label, float("nan")))

        try:
            print(f"  → 运行 CLI count bs={label} ...", end=" ", flush=True)
            t = bench_cli_count(path, bs)
            print(f"{fmt_sec(t)}")
            cli_results.append((label, t))
        except Exception as e:
            print(f"失败 ({e})")
            cli_results.append((label, float("nan")))

    # 把 bs=1M 的流式结果也加进来（和全量是同一回事）
    if results:
        # 找到 stream_xlsx_py (bs=1M) 的结果
        for name, t in results:
            if "bs=1M" in name:
                stream_results.append(("1,000,000", t))
                break

    print()
    print("-" * 50)
    print(f"{'batch_size':<12} {'Python 绑定':>18} {'CLI count':>18}")
    print("-" * 50)
    for (bs1, t1), (bs2, t2) in zip(stream_results, cli_results):
        assert bs1 == bs2, "batch_size 对齐错误"
        s1 = fmt_sec(t1) if not (t1 != t1) else "N/A"  # nan check
        s2 = fmt_sec(t2) if not (t2 != t2) else "N/A"
        print(f"{bs1:<12} {s1:>18} {s2:>18}")
    print("-" * 50)


if __name__ == "__main__":
    main()
