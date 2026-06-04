def main():
    import time

    import stream_xlsx_py

    start = time.time()
    # 用我们的 Rust 流式库读取
    print("=== stream_xlsx (惰性迭代器) ===")
    reader = stream_xlsx_py.read_xlsx(
        "test_100w_60c.xlsx", batch_size=100000, fast=True
    )
    count = 0
    for df in reader:
        print(df)
    print(count, time.time() - start)


if __name__ == "__main__":
    main()
