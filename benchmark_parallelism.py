#!/usr/bin/env python3
"""Benchmark fast mode with different --fast-parallelism values to find optimal concurrency."""

import subprocess
import sys
import time
from pathlib import Path


def run(parallelism: int, file: Path, batch_size: int = 10000) -> float:
    cmd = [
        "./target/release/sxlsx",
        "-B",
        str(batch_size),
        "--fast",
        "--fast-parallelism",
        str(parallelism),
        "test",
        "count",
        str(file),
    ]
    print(f"  → parallelism={parallelism:>2}  {' '.join(cmd)}")

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

    # 测试的并发数范围，覆盖单核到超订
    parallelism_values = [1, 2, 3, 4, 5, 6, 7, 8]
    batch_size = 10000  # 固定 batch size，减少变量

    print(f"\n{'=' * 60}")
    print(f"Fast mode parallelism benchmark")
    print(f"File: {file}")
    print(f"Batch size: {batch_size}")
    print(f"{'=' * 60}")

    results = []
    for p in parallelism_values:
        elapsed = run(p, file, batch_size)
        results.append((p, elapsed))

    # 找出最佳
    best = min(results, key=lambda x: x[1])

    print(f"\n{'=' * 60}")
    print(f"Summary (sorted by time, ascending)")
    print(f"{'=' * 60}")
    for p, t in sorted(results, key=lambda x: x[1]):
        marker = " <-- BEST" if p == best[0] else ""
        print(f"  parallelism={p:>2}  time={t:.3f}s{marker}")
    print(f"{'=' * 60}")

    # 保存 CSV
    csv_path = Path("benchmark_parallelism_results.csv")
    with open(csv_path, "w") as f:
        f.write("parallelism,elapsed_sec\n")
        for p, t in results:
            f.write(f"{p},{t:.3f}\n")
    print(f"Results saved to {csv_path}")


if __name__ == "__main__":
    main()
