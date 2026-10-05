use std::{
    error::Error,
    fmt,
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use polars::prelude::{DataType, PlSmallStr, Schema, SchemaRef};
use stream_xlsx::{
    SchemaMismatch, StrictDataFrameIter, StrictNumericReadAdapter, StrictNumericReadCell,
    StrictNumericReadValue, StrictReadError, StrictReadErrorKind, workbook::XlsxWorkbook,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct XlsxFixture(PathBuf);

impl XlsxFixture {
    fn new(sheets: &[(&str, &str)]) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "stream_xlsx_numeric_adapter_{}_{}.xlsx",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut workbook_sheets = String::new();
        let mut relationships = String::new();
        for (index, (name, _)) in sheets.iter().enumerate() {
            let sheet_id = index + 1;
            workbook_sheets.push_str(&format!(
                "<sheet name=\"{name}\" sheetId=\"{sheet_id}\" r:id=\"rId{sheet_id}\"/>"
            ));
            relationships.push_str(&format!(
                "<Relationship Id=\"rId{sheet_id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{sheet_id}.xml\"/>"
            ));
        }
        let workbook_xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>{workbook_sheets}</sheets></workbook>"
        );
        let rels_xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{relationships}</Relationships>"
        );

        let file = File::create(&path)?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        write_entry(
            &mut zip,
            "xl/workbook.xml",
            workbook_xml.as_bytes(),
            options,
        )?;
        write_entry(
            &mut zip,
            "xl/_rels/workbook.xml.rels",
            rels_xml.as_bytes(),
            options,
        )?;
        for (index, (_, xml)) in sheets.iter().enumerate() {
            write_entry(
                &mut zip,
                &format!("xl/worksheets/sheet{}.xml", index + 1),
                xml.as_bytes(),
                options,
            )?;
        }
        zip.finish()?;
        Ok(Self(path))
    }

    fn open(&self) -> anyhow::Result<Arc<XlsxWorkbook>> {
        Ok(Arc::new(XlsxWorkbook::open(&self.0)?))
    }
}

impl Drop for XlsxFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_entry(
    zip: &mut ZipWriter<File>,
    name: &str,
    contents: &[u8],
    options: SimpleFileOptions,
) -> anyhow::Result<()> {
    zip.start_file(name, options)?;
    zip.write_all(contents)?;
    Ok(())
}

fn worksheet(dimension: &str, rows: &str) -> String {
    format!(
        "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"{dimension}\"/><sheetData>{rows}</sheetData></worksheet>"
    )
}

fn row(number: u32, cells: &str) -> String {
    format!("<row r=\"{number}\">{cells}</row>")
}

fn inline(address: &str, value: &str) -> String {
    format!("<c r=\"{address}\" t=\"inlineStr\"><is><t>{value}</t></is></c>")
}

fn number(address: &str, raw: &str) -> String {
    format!("<c r=\"{address}\"><v>{raw}</v></c>")
}

fn text(address: &str, value: &str) -> String {
    format!("<c r=\"{address}\" t=\"str\"><v>{value}</v></c>")
}

fn schema(fields: impl IntoIterator<Item = (&'static str, DataType)>) -> SchemaRef {
    Arc::new(
        fields
            .into_iter()
            .map(|(name, dtype)| (PlSmallStr::from_static(name), dtype))
            .collect::<Schema>(),
    )
}

struct FunctionAdapter<F>(F);

impl<F> StrictNumericReadAdapter for FunctionAdapter<F>
where
    F: for<'a> Fn(StrictNumericReadCell<'a>) -> anyhow::Result<Option<StrictNumericReadValue>>
        + Send
        + Sync,
{
    fn read_numeric(
        &self,
        cell: StrictNumericReadCell<'_>,
    ) -> anyhow::Result<Option<StrictNumericReadValue>> {
        (self.0)(cell)
    }
}

fn adapter<F>(function: F) -> Arc<dyn StrictNumericReadAdapter>
where
    F: for<'a> Fn(StrictNumericReadCell<'a>) -> anyhow::Result<Option<StrictNumericReadValue>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(FunctionAdapter(function))
}

fn strict_reader(
    fixture: &XlsxFixture,
    sheet: &str,
    schema: SchemaRef,
    fast: bool,
) -> anyhow::Result<StrictDataFrameIter> {
    StrictDataFrameIter::from_workbook_with_schema(
        Some(8),
        fixture.open()?,
        Some(sheet),
        None,
        true,
        None,
        schema,
        fast,
        None,
    )
}

#[test]
fn fast_and_slow_adapters_receive_raw_values_and_schema_mapping() -> anyhow::Result<()> {
    let headers = format!("{}{}", inline("A1", "b"), inline("B1", "a"));
    let values = format!(
        "{}{}",
        number("A2", "9007199254740993"),
        number("B2", "1.2300")
    );
    let xml = worksheet("A1:B2", &format!("{}{}", row(1, &headers), row(2, &values)));
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let expected = schema([("a", DataType::Decimal(6, 2)), ("b", DataType::Float64)]);

    for fast in [false, true] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed_calls = Arc::clone(&calls);
        let numeric_adapter = adapter(move |cell| {
            observed_calls.lock().unwrap().push((
                cell.field_name.to_string(),
                cell.output_column,
                cell.target_type.clone(),
                cell.row,
                cell.column,
                cell.raw_numeric_lexeme.to_string(),
            ));
            let result = match (cell.field_name, cell.target_type) {
                ("a", DataType::Decimal(6, 2)) => {
                    Some(StrictNumericReadValue::DecimalCoefficient(123))
                }
                ("b", DataType::Float64) => {
                    Some(StrictNumericReadValue::Float64(9_007_199_254_740_992.0))
                }
                _ => return Err(anyhow::anyhow!("unexpected field or target type")),
            };
            Ok(result)
        });
        let reader = strict_reader(&fixture, "Sheet1", Arc::clone(&expected), fast)?
            .with_numeric_read_adapter(numeric_adapter)?;
        let batches = reader.collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(batches.len(), 1, "fast={fast}");
        let frame = &batches[0];
        assert_eq!(
            frame
                .get_column_names_owned()
                .iter()
                .map(PlSmallStr::as_str)
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "fast={fast}"
        );
        assert_eq!(
            frame.dtypes(),
            vec![DataType::Decimal(6, 2), DataType::Float64]
        );
        assert_eq!(frame.column("a")?.decimal()?.physical().get(0), Some(123));
        assert_eq!(
            frame.column("b")?.f64()?.get(0),
            Some(9_007_199_254_740_992.0)
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (
                    "b".into(),
                    1,
                    DataType::Float64,
                    1,
                    0,
                    "9007199254740993".into(),
                ),
                (
                    "a".into(),
                    0,
                    DataType::Decimal(6, 2),
                    1,
                    1,
                    "1.2300".into(),
                ),
            ],
            "fast={fast}"
        );
    }
    Ok(())
}

#[test]
fn decimal_override_uses_target_coefficient_and_respects_precision() -> anyhow::Result<()> {
    let cells = format!("{}{}", inline("A1", "amount"), number("A2", "1.239"));
    let xml = worksheet("A1:A2", &row(1, &cells));
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let expected = schema([("amount", DataType::Decimal(5, 2))]);
    let numeric_adapter = adapter(|cell| {
        assert_eq!(cell.field_name, "amount");
        assert_eq!(cell.target_type, &DataType::Decimal(5, 2));
        assert_eq!(cell.raw_numeric_lexeme, "1.239");
        Ok(Some(StrictNumericReadValue::DecimalCoefficient(124)))
    });
    let batches = strict_reader(&fixture, "Sheet1", Arc::clone(&expected), false)?
        .with_numeric_read_adapter(numeric_adapter)?
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(
        batches[0].column("amount")?.decimal()?.physical().get(0),
        Some(124)
    );

    let too_wide = adapter(|_| Ok(Some(StrictNumericReadValue::DecimalCoefficient(100_000))));
    let error = strict_reader(&fixture, "Sheet1", expected, false)?
        .with_numeric_read_adapter(too_wide)?
        .collect::<anyhow::Result<Vec<_>>>()
        .unwrap_err();
    assert!(format!("{error:#}").contains("固定类型或范围"), "{error:#}");
    Ok(())
}

#[test]
fn float_override_allows_integer_rounding_but_not_nonfinite_or_underflow() -> anyhow::Result<()> {
    let expected = schema([("amount", DataType::Float64)]);
    let xml = worksheet(
        "A1:A2",
        &row(
            1,
            &format!(
                "{}{}",
                inline("A1", "amount"),
                number("A2", "9007199254740993")
            ),
        ),
    );
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let numeric_adapter = adapter(|cell| {
        assert_eq!(cell.raw_numeric_lexeme, "9007199254740993");
        Ok(Some(StrictNumericReadValue::Float64(
            9_007_199_254_740_992.0,
        )))
    });
    let batches = strict_reader(&fixture, "Sheet1", Arc::clone(&expected), false)?
        .with_numeric_read_adapter(numeric_adapter)?
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(
        batches[0].column("amount")?.f64()?.get(0),
        Some(9_007_199_254_740_992.0)
    );

    for (raw, override_value) in [
        ("1", f64::INFINITY),
        ("1e309", 1.0),
        ("1e-400", 0.0),
        ("1e-400", 1.0),
    ] {
        let xml = worksheet(
            "A1:A2",
            &row(
                1,
                &format!("{}{}", inline("A1", "amount"), number("A2", raw)),
            ),
        );
        let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
        let numeric_adapter =
            adapter(move |_| Ok(Some(StrictNumericReadValue::Float64(override_value))));
        let error = strict_reader(&fixture, "Sheet1", Arc::clone(&expected), false)?
            .with_numeric_read_adapter(numeric_adapter)?
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap_err();
        if raw == "1e309" || raw == "1e-400" {
            assert!(matches!(
                &error.downcast_ref::<StrictReadError>().unwrap().kind,
                StrictReadErrorKind::SchemaMismatch(SchemaMismatch::Float64LexemeLoss { .. })
            ));
        } else {
            assert!(format!("{error:#}").contains("固定类型或范围"), "{error:#}");
        }
    }
    Ok(())
}

#[test]
fn numeric_looking_physical_string_never_reaches_adapter_or_numeric_parser() -> anyhow::Result<()> {
    let xml = worksheet(
        "A1:A2",
        &row(
            1,
            &format!("{}{}", inline("A1", "amount"), text("A2", "00123")),
        ),
    );
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let expected = schema([("amount", DataType::Decimal(5, 0))]);

    for fast in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&calls);
        let numeric_adapter = adapter(move |_| {
            observed_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Some(StrictNumericReadValue::DecimalCoefficient(123)))
        });
        let mut reader = strict_reader(&fixture, "Sheet1", Arc::clone(&expected), fast)?
            .with_numeric_read_adapter(numeric_adapter)?;
        let error = reader.next().expect("expected one result").unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 0, "fast={fast}");
        assert!(matches!(
            &error.downcast_ref::<StrictReadError>().unwrap().kind,
            StrictReadErrorKind::SchemaMismatch(SchemaMismatch::CellPhysicalType {
                actual, ..
            }) if actual == "String"
        ));
    }
    Ok(())
}

#[test]
fn adapter_is_fixed_before_reading_and_prevents_sheet_switching() -> anyhow::Result<()> {
    let first = worksheet(
        "A1:A2",
        &row(
            1,
            &format!("{}{}", inline("A1", "amount"), number("A2", "1")),
        ),
    );
    let second = worksheet(
        "A1:A2",
        &row(
            1,
            &format!("{}{}", inline("A1", "amount"), number("A2", "2")),
        ),
    );
    let fixture = XlsxFixture::new(&[("First", &first), ("Second", &second)])?;
    let expected = schema([("amount", DataType::Decimal(5, 0))]);
    let numeric_adapter = adapter(|_| Ok(None));
    let mut reader = strict_reader(&fixture, "First", Arc::clone(&expected), false)?
        .with_numeric_read_adapter(Arc::clone(&numeric_adapter))?;
    assert!(reader.select_sheet(Some("Second"), None).is_err());

    let mut reader = strict_reader(&fixture, "First", expected, false)?
        .with_numeric_read_adapter(Arc::clone(&numeric_adapter))?;
    assert!(reader.next().expect("expected one batch").is_ok());
    let result = reader.with_numeric_read_adapter(numeric_adapter);
    assert!(result.is_err());
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct AdapterFailure;

impl fmt::Display for AdapterFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("intentional adapter failure")
    }
}

impl Error for AdapterFailure {}

#[test]
fn adapter_error_keeps_its_type_through_sheet_context() -> anyhow::Result<()> {
    let xml = worksheet(
        "A1:A2",
        &row(
            1,
            &format!("{}{}", inline("A1", "amount"), number("A2", "1")),
        ),
    );
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let numeric_adapter = adapter(|_| Err(anyhow::Error::new(AdapterFailure)));
    let mut reader = strict_reader(
        &fixture,
        "Sheet1",
        schema([("amount", DataType::Decimal(5, 0))]),
        false,
    )?
    .with_numeric_read_adapter(numeric_adapter)?;
    let error = reader.next().expect("expected one result").unwrap_err();
    assert_eq!(
        error.downcast_ref::<AdapterFailure>(),
        Some(&AdapterFailure)
    );
    assert!(format!("{error:#}").contains("Sheet1"));
    Ok(())
}
