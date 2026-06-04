#!/usr/bin/env python3
"""Benchmark fast mode with different temp_size and buf_size values (fixed parallelism=8)."""

import subprocess
import sys
import time
from pathlib import Path


def run(temp_kb: int, buf_kb: int, file: Path, parallelism: int = 8, batch_size: int = 10000) -> float:
    cmd = [
        "./target/release/sxlsx",
        "-B", str(batch_size),
        "--fast",
        "--fast-parallelism", str(parallelism),
        "--fast-temp-kb", str(temp_kb),
        "--fast-buf-kb", str(buf_kb),
        "test", "count", str(file),
    ]
    print(f"  → temp={temp_kb:>5}KB buf={buf_kb:>5}KB {' '.join(cmd)}")

    start = time.perf_counter()
    result = subprocess.run(cmd, capture_output=True, text=True)
    elapsed = time.perf_counter() - start

    status = "✅" if result.returncode == 0 else "❌"
    print(f"    {status} time={elapsed:.3f}s  stdout={result.stdout.strip()}")
    if result.stderr:
        print(f"       stderr: {result.stderr.strip()}")

    return elapsed


def main():
    file = Path("test_100w_60c.xlsx")
    if not file.exists():
        print(f"File not found: {file}")
        sys.exit(1)

    # 测试范围
    temp_sizes = [256, 512, 1024, 2048, 4096, 8192]
    buf_sizes = [256, 512, 1024, 2048, 4096, 8192]
    parallelism = 8
    batch_size = 10000

    print(f"\n{'=' * 60}")
    print(f"Fast mode buffer benchmark (parallelism={parallelism})")
    print(f"File: {file}")
    print(f"Batch size: {batch_size}")
    print(f"{'=' * 60}")

    results = []
    for temp_kb in temp_sizes:
        for buf_kb in buf_sizes:
            elapsed = run(temp_kb, buf_kb, file, parallelism, batch_size)
            results.append((temp_kb, buf_kb, elapsed))

    # 找出最佳
    best = min(results, key=lambda x: x[2])

    print(f"\n{'=' * 60}")
    print(f"Summary (sorted by time, ascending)")
    print(f"{'=' * 60}")
    for temp_kb, buf_kb, t in sorted(results, key=lambda x: x[2]):
        marker = " <-- BEST" if (temp_kb, buf_kb) == (best[0], best[1]) else ""
        print(f"  temp={temp_kb:>5}KB buf={buf_kb:>5}KB  time={t:.3f}s{marker}")
    print(f"{'=' * 60}")

    # 保存 CSV
    csv_path = Path("benchmark_buffer_results.csv")
    with open(csv_path, "w") as f:
        f.write("temp_kb,buf_kb,elapsed_sec\n")
        for temp_kb, buf_kb, t in results:
            f.write(f"{temp_kb},{buf_kb},{t:.3f}\n")
    print(f"Results saved to {csv_path}")


if __name__ == "__main__":
    main()
