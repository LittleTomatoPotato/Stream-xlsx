def main():
    import polars as pl

    df = pl.read_excel("inlineStr_cdata.xlsx")
    print(df)


if __name__ == "__main__":
    main()
