#!/usr/bin/env python3
"""Python 环境下对比 stream_xlsx_py 与 polars 原生读取方式。"""

import csv
import json
import subprocess
import sys
import time
from pathlib import Path

import matplotlib
import matplotlib.pyplot as plt
import psutil

matplotlib.use("Agg")

TEST_FILE = Path("test_100w_60c.xlsx")


def run_subprocess_test(name: str, code: str) -> dict:
    """启动独立 Python 子进程执行测试代码，主进程监控其内存。"""
    print(f"  → {name}")
    cmd = [sys.executable, "-c", code]

    start = time.perf_counter()
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    p = psutil.Process(proc.pid)

    timestamps = []
    rss_series = []
    peak_rss_mb = 0.0

    try:
        while proc.poll() is None:
            try:
                mem = p.memory_info().rss
                peak_rss_mb = max(peak_rss_mb, mem / 1024 / 1024)
                timestamps.append(time.perf_counter() - start)
                rss_series.append(mem / 1024 / 1024)
            except psutil.NoSuchProcess:
                break
            time.sleep(0.05)
    finally:
        proc.wait()

    elapsed = time.perf_counter() - start
    stdout = (
        proc.stdout.read().decode("utf-8", errors="replace").strip()
        if proc.stdout
        else ""
    )
    stderr = (
        proc.stderr.read().decode("utf-8", errors="replace").strip()
        if proc.stderr
        else ""
    )

    return {
        "name": name,
        "elapsed_sec": round(elapsed, 2),
        "peak_rss_mb": round(peak_rss_mb, 1),
        "timestamps": [round(t, 2) for t in timestamps],
        "rss_series": [round(r, 1) for r in rss_series],
        "stdout": stdout,
        "stderr": stderr,
        "returncode": proc.returncode,
    }


def make_test_code_stream_xlsx(batch_size: int, reader: str) -> str:
    return f'''
import stream_xlsx_py as sx
for df in sx.read_xlsx("{TEST_FILE}", batch_size={batch_size}, reader="{reader}"):
    pass
print("done")
'''


def make_test_code_polars(engine: str) -> str:
    return f'''
import polars as pl
df = pl.read_excel("{TEST_FILE}", engine="{engine}")
print(df.shape)
'''


def main():
    if not TEST_FILE.exists():
        print(f"File not found: {TEST_FILE}")
        sys.exit(1)

    results = []

    # 1. stream_xlsx_py 不同 reader / batch_size（含全量 1M）
    print("\n=== stream_xlsx_py ===")
    for reader in ["default", "lm"]:
        for bs in [10_000, 50_000, 100_000, 1_000_000]:
            name = f"stream_xlsx_py reader={reader} batch={bs}"
            code = make_test_code_stream_xlsx(bs, reader)
            res = run_subprocess_test(name, code)
            results.append(res)
            print(
                f"    {'✅' if res['returncode'] == 0 else '❌'} {name}: {res['elapsed_sec']:.2f}s  {res['peak_rss_mb']:.1f}MB"
            )
            if res["stderr"]:
                print(f"       stderr: {res['stderr'][:200]}")

    # 2. polars calamine
    print("\n=== polars calamine ===")
    res = run_subprocess_test(
        "polars calamine",
        make_test_code_polars("calamine"),
    )
    results.append(res)
    print(
        f"    {'✅' if res['returncode'] == 0 else '❌'} polars calamine: {res['elapsed_sec']:.2f}s  {res['peak_rss_mb']:.1f}MB"
    )

    # 3. polars xlsx2csv (optional, may be slow)
    print("\n=== polars xlsx2csv ===")
    res = run_subprocess_test(
        "polars xlsx2csv",
        make_test_code_polars("xlsx2csv"),
    )
    results.append(res)
    print(
        f"    {'✅' if res['returncode'] == 0 else '❌'} polars xlsx2csv: {res['elapsed_sec']:.2f}s  {res['peak_rss_mb']:.1f}MB"
    )

    # Save data
    out_dir = Path("benchmark_python_plots")
    out_dir.mkdir(exist_ok=True)

    with open("benchmark_python.json", "w") as f:
        json.dump(results, f, indent=2)

    with open("benchmark_python.csv", "w", newline="") as f:
        writer = csv.DictWriter(
            f, fieldnames=["name", "elapsed_sec", "peak_rss_mb", "returncode"]
        )
        writer.writeheader()
        for r in results:
            writer.writerow({k: r[k] for k in writer.fieldnames})

    # Plot
    fig, ax = plt.subplots(figsize=(12, 6))
    colors = plt.cm.tab10

    for i, r in enumerate(results):
        ax.plot(
            r["timestamps"],
            r["rss_series"],
            label=r["name"],
            color=colors(i),
            linewidth=1.2,
        )

    ax.set_title("Python Environment: Memory Usage Over Time")
    ax.set_xlabel("Time (s)")
    ax.set_ylabel("RSS (MB)")
    ax.legend(loc="upper right", fontsize=8)
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(out_dir / "python_memory_comparison.png", dpi=150)
    plt.close(fig)

    print(f"\nPlots saved to {out_dir}/")
    print(f"Data saved to benchmark_python.json / benchmark_python.csv")

    # Summary table
    print("\n" + "=" * 80)
    print(f"{'Name':<45} {'Time(s)':>10} {'Peak(MB)':>12}")
    print("-" * 80)
    for r in results:
        print(f"{r['name']:<45} {r['elapsed_sec']:>10.2f} {r['peak_rss_mb']:>12.1f}")
    print("=" * 80)


if __name__ == "__main__":
    main()
