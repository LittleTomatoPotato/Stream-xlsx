#!/usr/bin/env python3
"""Benchmark both readers across batch sizes, measuring time-series RSS."""

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


def run_benchmark(reader: str, batch_size: int, file: Path) -> dict:
    cmd = [
        "./target/release/sxlsx",
        "-B",
        str(batch_size),
        "test",
        "count",
        str(file),
    ]
    print(f"  → {' '.join(cmd)}")

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
        "reader": reader,
        "batch_size": batch_size,
        "elapsed_sec": round(elapsed, 2),
        "peak_rss_mb": round(peak_rss_mb, 1),
        "timestamps": [round(t, 2) for t in timestamps],
        "rss_series": [round(r, 1) for r in rss_series],
        "stdout": stdout,
        "stderr": stderr,
        "returncode": proc.returncode,
    }


def plot_all(results: list, out_dir: Path):
    out_dir.mkdir(exist_ok=True)
    readers = ["lm"]
    colors = plt.cm.tab10

    # 1. Per-reader combined figure (all batch sizes on one plot)
    for r_idx, reader in enumerate(readers):
        fig, ax = plt.subplots(figsize=(10, 5))
        reader_results = [res for res in results if res["reader"] == reader]
        for i, res in enumerate(reader_results):
            ax.plot(
                res["timestamps"],
                res["rss_series"],
                label=f"batch={res['batch_size']}",
                color=colors(i),
                linewidth=1.2,
            )
        ax.set_title(f"Memory Usage Over Time — Reader: {reader}")
        ax.set_xlabel("Time (s)")
        ax.set_ylabel("RSS (MB)")
        ax.legend(loc="upper right")
        ax.grid(True, alpha=0.3)
        fig.tight_layout()
        fig.savefig(out_dir / f"memory_{reader}.png", dpi=150)
        plt.close(fig)

    # 2. Per-batch-size comparison figure (default vs lm)
    batch_sizes = sorted({res["batch_size"] for res in results})
    for bs in batch_sizes:
        fig, ax = plt.subplots(figsize=(10, 5))
        for r_idx, reader in enumerate(readers):
            res = next(
                (r for r in results if r["reader"] == reader and r["batch_size"] == bs),
                None,
            )
            if not res:
                continue
            if res:
                ax.plot(
                    res["timestamps"],
                    res["rss_series"],
                    label=f"{reader}",
                    color=colors(r_idx),
                    linewidth=1.5,
                )
        ax.set_title(f"Memory Usage Over Time — Reader: lm, Batch Size: {bs}")
        ax.set_xlabel("Time (s)")
        ax.set_ylabel("RSS (MB)")
        ax.legend(loc="upper right")
        ax.grid(True, alpha=0.3)
        fig.tight_layout()
        fig.savefig(out_dir / f"memory_batch_{bs}.png", dpi=150)
        plt.close(fig)

    # 3. 2x6 grid: every condition gets its own mini-plot
    fig, axes = plt.subplots(1, 6, figsize=(24, 4), sharey=True)
    for r_idx, reader in enumerate(readers):
        for b_idx, bs in enumerate(batch_sizes):
            ax = axes[b_idx]
            res = next(
                (r for r in results if r["reader"] == reader and r["batch_size"] == bs),
                None,
            )
            if res:
                ax.plot(
                    res["timestamps"],
                    res["rss_series"],
                    color=colors(r_idx),
                    linewidth=1,
                )
                ax.set_title(f"{reader}\nbatch={bs}", fontsize=10)
                ax.set_xlabel("Time (s)", fontsize=8)
                if b_idx == 0:
                    ax.set_ylabel("RSS (MB)", fontsize=8)
                ax.grid(True, alpha=0.3)
    fig.suptitle("Memory Usage Over Time — All Conditions", fontsize=14)
    fig.tight_layout(rect=[0, 0, 1, 0.96])
    fig.savefig(out_dir / "memory_all_grid.png", dpi=150)
    plt.close(fig)


def main():
    file = Path("test_100w_60c.xlsx")
    if not file.exists():
        print(f"File not found: {file}")
        sys.exit(1)

    readers = ["lm"]
    batch_sizes = [1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000]
    results = []

    for reader in readers:
        print(f"\n{'=' * 60}")
        print(f"Reader: {reader}")
        print(f"{'=' * 60}")
        for bs in batch_sizes:
            result = run_benchmark(reader, bs, file)
            results.append(result)
            status = "✅" if result["returncode"] == 0 else "❌"
            print(
                f"    {status} batch={bs:>7}  time={result['elapsed_sec']:>6.2f}s  "
                f"peak_mem={result['peak_rss_mb']:>8.1f}MB  stdout={result["stdout"]}"
            )
            if result["stderr"]:
                print(f"       stderr: {result['stderr']}")

    # Save raw time-series data
    ts_path = Path("benchmark_timeseries.json")
    with ts_path.open("w") as f:
        json.dump(results, f, indent=2)
    print(f"\nTime-series data saved to {ts_path}")

    # Save summary CSV
    csv_path = Path("benchmark_results.csv")
    with csv_path.open("w", newline="") as f:
        writer = csv.DictWriter(
            f,
            fieldnames=[
                "reader",
                "batch_size",
                "elapsed_sec",
                "peak_rss_mb",
                "returncode",
                "stdout",
            ],
        )
        writer.writeheader()
        for r in results:
            writer.writerow({k: r[k] for k in writer.fieldnames})
    print(f"Summary CSV saved to {csv_path}")

    # Generate plots
    out_dir = Path("benchmark_plots")
    plot_all(results, out_dir)
    print(f"Plots saved to {out_dir}/")

    # Print summary table
    print("\n" + "=" * 70)
    print(f"{'Reader':<10} {'Batch':>8} {'Time(s)':>10} {'Peak(MB)':>12}")
    print("-" * 70)
    for r in results:
        print(
            f"{r['reader']:<10} {r['batch_size']:>8} {r['elapsed_sec']:>10.2f} {r['peak_rss_mb']:>12.1f}"
        )
    print("=" * 70)


if __name__ == "__main__":
    main()
