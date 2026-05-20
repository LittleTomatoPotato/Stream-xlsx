def main():
    import time

    import stream_xlsx_py

    start = time.time()
    # 用我们的 Rust 流式库读取
    print("=== stream_xlsx (惰性迭代器) ===")
    reader = stream_xlsx_py.read_xlsx("target/release/test_data_1m.xslx")
    count = 0
    for df in reader:
        count += 1
        # print(df)
    print(count, time.time() - start)


def polars_read():
    import time

    import polars as pl

    start = time.time()
    _df = pl.read_excel("target/release/test_data_1m.xslx")
    print(time.time() - start)


if __name__ == "__main__":
    main()
    polars_read()
