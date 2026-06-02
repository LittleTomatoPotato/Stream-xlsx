#!/usr/bin/env python3
"""Benchmark both modes (low-memory vs fast) across batch sizes, measuring time-series RSS."""

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


def run_benchmark(mode: str, batch_size: int, file: Path) -> dict:
    cmd = [
        "./target/release/sxlsx",
        "-B",
        str(batch_size),
    ]
    if mode == "fast":
        cmd.append("--fast")
    cmd.extend(["test", "count", str(file)])
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
        "mode": mode,
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
    modes = ["default", "fast"]
    colors = {"default": "#1f77b4", "fast": "#ff7f0e"}

    # 1. Per-mode combined figure (all batch sizes on one plot)
    for mode in modes:
        fig, ax = plt.subplots(figsize=(10, 5))
        mode_results = [res for res in results if res["mode"] == mode]
        for i, res in enumerate(mode_results):
            ax.plot(
                res["timestamps"],
                res["rss_series"],
                label=f"batch={res['batch_size']}",
                color=plt.cm.tab10(i),
                linewidth=1.2,
            )
        ax.set_title(f"Memory Usage Over Time — Mode: {mode}")
        ax.set_xlabel("Time (s)")
        ax.set_ylabel("RSS (MB)")
        ax.legend(loc="upper right")
        ax.grid(True, alpha=0.3)
        fig.tight_layout()
        fig.savefig(out_dir / f"memory_{mode}.png", dpi=150)
        plt.close(fig)

    # 2. Per-batch-size comparison figure (default vs fast)
    batch_sizes = sorted({res["batch_size"] for res in results})
    for bs in batch_sizes:
        fig, ax = plt.subplots(figsize=(10, 5))
        for mode in modes:
            res = next(
                (r for r in results if r["mode"] == mode and r["batch_size"] == bs),
                None,
            )
            if res:
                ax.plot(
                    res["timestamps"],
                    res["rss_series"],
                    label=f"{mode}",
                    color=colors[mode],
                    linewidth=1.5,
                )
        ax.set_title(f"Memory Usage Over Time — Batch Size: {bs}")
        ax.set_xlabel("Time (s)")
        ax.set_ylabel("RSS (MB)")
        ax.legend(loc="upper right")
        ax.grid(True, alpha=0.3)
        fig.tight_layout()
        fig.savefig(out_dir / f"memory_batch_{bs}.png", dpi=150)
        plt.close(fig)

    # 3. Bar chart: time & memory comparison per batch size
    fig, axes = plt.subplots(1, 2, figsize=(14, 5))

    # Time bar chart
    ax = axes[0]
    x = range(len(batch_sizes))
    width = 0.35
    default_times = [
        next((r["elapsed_sec"] for r in results if r["mode"] == "default" and r["batch_size"] == bs), 0)
        for bs in batch_sizes
    ]
    fast_times = [
        next((r["elapsed_sec"] for r in results if r["mode"] == "fast" and r["batch_size"] == bs), 0)
        for bs in batch_sizes
    ]
    ax.bar([i - width / 2 for i in x], default_times, width, label="default", color=colors["default"])
    ax.bar([i + width / 2 for i in x], fast_times, width, label="fast", color=colors["fast"])
    ax.set_xlabel("Batch Size")
    ax.set_ylabel("Time (s)")
    ax.set_title("Elapsed Time Comparison")
    ax.set_xticks(x)
    ax.set_xticklabels(batch_sizes, rotation=45, ha="right")
    ax.legend()
    ax.grid(True, alpha=0.3, axis="y")

    # Memory bar chart
    ax = axes[1]
    default_mem = [
        next((r["peak_rss_mb"] for r in results if r["mode"] == "default" and r["batch_size"] == bs), 0)
        for bs in batch_sizes
    ]
    fast_mem = [
        next((r["peak_rss_mb"] for r in results if r["mode"] == "fast" and r["batch_size"] == bs), 0)
        for bs in batch_sizes
    ]
    ax.bar([i - width / 2 for i in x], default_mem, width, label="default", color=colors["default"])
    ax.bar([i + width / 2 for i in x], fast_mem, width, label="fast", color=colors["fast"])
    ax.set_xlabel("Batch Size")
    ax.set_ylabel("Peak RSS (MB)")
    ax.set_title("Peak Memory Comparison")
    ax.set_xticks(x)
    ax.set_xticklabels(batch_sizes, rotation=45, ha="right")
    ax.legend()
    ax.grid(True, alpha=0.3, axis="y")

    fig.tight_layout()
    fig.savefig(out_dir / "comparison_bars.png", dpi=150)
    plt.close(fig)

    # 4. 2xN grid: every condition gets its own mini-plot
    fig, axes = plt.subplots(2, len(batch_sizes), figsize=(4 * len(batch_sizes), 8), sharey=True)
    for m_idx, mode in enumerate(modes):
        for b_idx, bs in enumerate(batch_sizes):
            ax = axes[m_idx][b_idx]
            res = next(
                (r for r in results if r["mode"] == mode and r["batch_size"] == bs),
                None,
            )
            if res:
                ax.plot(
                    res["timestamps"],
                    res["rss_series"],
                    color=colors[mode],
                    linewidth=1,
                )
                ax.set_title(f"{mode}\nbatch={bs}", fontsize=10)
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

    modes = ["default", "fast"]
    batch_sizes = [1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000]
    results = []

    for mode in modes:
        print(f"\n{'=' * 60}")
        print(f"Mode: {mode}")
        print(f"{'=' * 60}")
        for bs in batch_sizes:
            result = run_benchmark(mode, bs, file)
            results.append(result)
            status = "✅" if result["returncode"] == 0 else "❌"
            print(
                f"    {status} batch={bs:>7}  time={result['elapsed_sec']:>6.2f}s  ",
                f"peak_mem={result['peak_rss_mb']:>8.1f}MB  stdout={result['stdout']}",
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
                "mode",
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
    print("\n" + "=" * 80)
    print(f"{'Mode':<10} {'Batch':>8} {'Time(s)':>10} {'Peak(MB)':>12} {'Speedup':>10}")
    print("-" * 80)
    for bs in batch_sizes:
        default_time = next(
            (r["elapsed_sec"] for r in results if r["mode"] == "default" and r["batch_size"] == bs), 0
        )
        fast_time = next(
            (r["elapsed_sec"] for r in results if r["mode"] == "fast" and r["batch_size"] == bs), 0
        )
        default_mem = next(
            (r["peak_rss_mb"] for r in results if r["mode"] == "default" and r["batch_size"] == bs), 0
        )
        fast_mem = next(
            (r["peak_rss_mb"] for r in results if r["mode"] == "fast" and r["batch_size"] == bs), 0
        )
        speedup = f"{default_time / fast_time:.2f}x" if fast_time > 0 else "N/A"
        print(f"{'default':<10} {bs:>8} {default_time:>10.2f} {default_mem:>12.1f}")
        print(f"{'fast':<10} {bs:>8} {fast_time:>10.2f} {fast_mem:>12.1f} {speedup:>10}")
        print("-" * 80)
    print("=" * 80)


if __name__ == "__main__":
    main()
