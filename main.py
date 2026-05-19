def main():
    import time

    import stream_xlsx

    start = time.time()
    # 用我们的 Rust 流式库读取
    print("=== stream_xlsx (惰性迭代器) ===")
    reader = stream_xlsx.read_xlsx("test_data.xlsx", batch_size=100000)
    count = 0
    for df in reader:
        count += 1
        # print(df)
    print(count, time.time() - start)


def polars_read():
    import time

    import polars as pl

    start = time.time()
    df = pl.read_excel("test_data.xlsx")
    print(time.time() - start)


if __name__ == "__main__":
    main()
    polars_read()
