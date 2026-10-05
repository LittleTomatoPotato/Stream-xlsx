use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use polars::prelude::{DataType, PlSmallStr, Schema, SchemaRef};
use stream_xlsx::utils::parse_a1;
use stream_xlsx::workbook::XlsxWorkbook;
use stream_xlsx::{
    HeaderInspectOptions, HeaderMergeOverflow, HeaderMergeRange, HeaderPlanError, HeaderWarning,
    SchemaMismatch, SheetPhysicalKind, StrictDataFrameIter, StrictReadError, StrictReadErrorKind,
    XlsxHeaderPath, XlsxHeaderPlan, inspect_header_plan, inspect_sheet_schema_with_header_plan,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct XlsxFixture(PathBuf);

impl XlsxFixture {
    fn new(sheets: &[(&str, &str)]) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "stream_xlsx_header_plan_{}_{}.xlsx",
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

fn worksheet(dimension: &str, rows: &str, merges: &[&str]) -> String {
    let merge_xml = if merges.is_empty() {
        String::new()
    } else {
        let items = merges
            .iter()
            .map(|reference| format!("<mergeCell ref=\"{reference}\"/>"))
            .collect::<String>();
        format!(
            "<mergeCells count=\"{}\">{items}</mergeCells>",
            merges.len()
        )
    };
    format!(
        "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"{dimension}\"/><sheetData>{rows}</sheetData>{merge_xml}</worksheet>"
    )
}

fn row(number: u32, cells: &str) -> String {
    format!("<row r=\"{number}\">{cells}</row>")
}

fn inline(address: &str, value: &str) -> String {
    format!("<c r=\"{address}\" t=\"inlineStr\"><is><t>{value}</t></is></c>")
}

fn number(address: &str, value: i64) -> String {
    format!("<c r=\"{address}\"><v>{value}</v></c>")
}

fn styled_blank(address: &str) -> String {
    format!("<c r=\"{address}\" s=\"1\"/>")
}

fn options(header_depth: u8) -> HeaderInspectOptions {
    HeaderInspectOptions {
        header_depth,
        ..HeaderInspectOptions::default()
    }
}

fn plan_for(
    fixture: &XlsxFixture,
    sheet: &str,
    options: HeaderInspectOptions,
) -> anyhow::Result<Arc<XlsxHeaderPlan>> {
    inspect_header_plan(fixture.open()?, Some(sheet), None, options)
}

fn assert_plan_error(
    result: anyhow::Result<Arc<XlsxHeaderPlan>>,
    matches: impl FnOnce(&HeaderPlanError) -> bool,
) {
    let error = result.err().expect("expected header-plan error");
    assert!(
        error.downcast_ref::<HeaderPlanError>().is_some_and(matches),
        "unexpected error: {error:#}"
    );
}

fn schema(fields: impl IntoIterator<Item = (&'static str, DataType)>) -> SchemaRef {
    Arc::new(
        fields
            .into_iter()
            .map(|(name, dtype)| (PlSmallStr::from_static(name), dtype))
            .collect::<Schema>(),
    )
}

#[test]
fn merges_and_unmerged_labels_produce_structured_paths() -> anyhow::Result<()> {
    let mut first = String::new();
    first.push_str(&inline("A1", "Sales"));
    first.push_str(&inline("C1", "Quarter"));
    first.push_str(&inline("D1", "Same"));
    first.push_str(&inline("E1", "a.b"));
    first.push_str(&inline("F1", "a"));
    first.push_str(&inline("G1", "Top"));
    let mut second = String::new();
    second.push_str(&inline("A2", "Online"));
    second.push_str(&inline("B2", "Retail"));
    second.push_str(&inline("D2", "Same"));
    second.push_str(&inline("E2", "c"));
    second.push_str(&inline("F2", "b.c"));
    second.push_str(&inline("H2", "Child"));
    let xml = worksheet(
        "A1:H2",
        &format!("{}{}", row(1, &first), row(2, &second)),
        &["A1:B1", "C1:C2"],
    );
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let plan = plan_for(&fixture, "Sheet1", options(2))?;

    let paths = plan
        .columns()
        .iter()
        .map(|column| column.path.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        paths[0],
        XlsxHeaderPath(vec!["Sales".into(), "Online".into()])
    );
    assert_eq!(
        paths[1],
        XlsxHeaderPath(vec!["Sales".into(), "Retail".into()])
    );
    assert_eq!(paths[2], XlsxHeaderPath(vec!["Quarter".into()]));
    assert_eq!(paths[3], XlsxHeaderPath(vec!["Same".into(), "Same".into()]));
    assert_eq!(paths[4], XlsxHeaderPath(vec!["a.b".into(), "c".into()]));
    assert_eq!(paths[5], XlsxHeaderPath(vec!["a".into(), "b.c".into()]));
    assert_eq!(paths[6], XlsxHeaderPath(vec!["Top".into()]));
    // H1 未合并且为空，因此不会从 G1 或其它邻近单元格补入标签。
    assert_eq!(paths[7], XlsxHeaderPath(vec!["Child".into()]));
    assert_eq!(
        plan.merges(),
        &[
            HeaderMergeRange {
                start: (0, 0),
                end: (0, 1),
            },
            HeaderMergeRange {
                start: (0, 2),
                end: (1, 2),
            },
        ]
    );

    // 若将路径拍平成字符串再按句点拆分，这两条路径会发生碰撞。
    let bound = plan.bind_fields(&[
        ("sales_online".into(), paths[0].clone()),
        ("sales_retail".into(), paths[1].clone()),
        ("quarter".into(), paths[2].clone()),
        ("same_text_twice".into(), paths[3].clone()),
        ("dot_in_top_label".into(), paths[4].clone()),
        ("dot_in_lower_label".into(), paths[5].clone()),
        ("unmerged_top".into(), paths[6].clone()),
        ("no_unmerged_fill".into(), paths[7].clone()),
    ])?;
    assert_eq!(bound.header_plan().columns().len(), 8);
    Ok(())
}

#[test]
fn skips_top_rows_and_format_only_rows_before_finding_header_start() -> anyhow::Result<()> {
    let rows = format!(
        "{}{}{}{}{}",
        row(1, &inline("A1", "Preamble")),
        row(2, &styled_blank("A2")),
        row(3, &inline("A3", "Group")),
        row(4, &inline("A4", "Id")),
        row(5, &number("A5", 41)),
    );
    let xml = worksheet("A1:A5", &rows, &[]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let plan = plan_for(
        &fixture,
        "Sheet1",
        HeaderInspectOptions {
            skip_top_rows: 1,
            header_depth: 2,
            ..HeaderInspectOptions::default()
        },
    )?;
    assert_eq!(plan.first_header_row(), 2);
    assert_eq!(plan.data_start_row(), 4);
    assert_eq!(
        plan.columns()[0].path,
        XlsxHeaderPath(vec!["Group".into(), "Id".into()])
    );
    Ok(())
}

#[test]
fn deepest_header_row_sets_right_edge_and_merge_overflow_is_explicit() -> anyhow::Result<()> {
    let rows = format!(
        "{}{}{}",
        row(1, &inline("A1", "Top")),
        row(
            2,
            &format!("{}{}", inline("A2", "Left"), inline("B2", "Right"))
        ),
        row(3, &number("A3", 7)),
    );
    let xml = worksheet("A1:C3", &rows, &["A1:C1"]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;

    assert_plan_error(plan_for(&fixture, "Sheet1", options(2)), |error| {
        matches!(
            error,
            HeaderPlanError::MergeOverflow {
                rightmost_column: 1,
                ..
            }
        )
    });

    let plan = plan_for(
        &fixture,
        "Sheet1",
        HeaderInspectOptions {
            merge_overflow: HeaderMergeOverflow::ClipAndWarn,
            ..options(2)
        },
    )?;
    assert_eq!(plan.columns().len(), 2);
    assert_eq!(
        plan.warnings(),
        &[HeaderWarning::MergeClipped {
            range: HeaderMergeRange {
                start: (0, 0),
                end: (0, 2),
            },
            rightmost_column: 1,
        }]
    );
    Ok(())
}

#[test]
fn rejects_cross_boundary_overlapping_and_duplicate_header_paths() -> anyhow::Result<()> {
    let cross_rows = format!(
        "{}{}{}",
        row(1, &inline("A1", "Top")),
        row(2, &inline("A2", "Bottom")),
        row(3, &number("A3", 1)),
    );
    let cross_xml = worksheet("A1:A3", &cross_rows, &["A1:A3"]);
    let cross_fixture = XlsxFixture::new(&[("Sheet1", &cross_xml)])?;
    assert_plan_error(plan_for(&cross_fixture, "Sheet1", options(2)), |error| {
        matches!(error, HeaderPlanError::MergeCrossesHeaderBoundary(_))
    });

    let overlap_rows = format!(
        "{}{}",
        row(1, &inline("A1", "Top")),
        row(
            2,
            &format!(
                "{}{}{}",
                inline("A2", "A"),
                inline("B2", "B"),
                inline("C2", "C")
            )
        ),
    );
    let overlap_xml = worksheet("A1:C2", &overlap_rows, &["A1:B1", "B1:C1"]);
    let overlap_fixture = XlsxFixture::new(&[("Sheet1", &overlap_xml)])?;
    assert_plan_error(plan_for(&overlap_fixture, "Sheet1", options(2)), |error| {
        matches!(error, HeaderPlanError::OverlappingMerges { .. })
    });

    let duplicate_rows = row(
        1,
        &format!("{}{}", inline("A1", "Same"), inline("B1", "Same")),
    );
    let duplicate_xml = worksheet("A1:B1", &duplicate_rows, &[]);
    let duplicate_fixture = XlsxFixture::new(&[("Sheet1", &duplicate_xml)])?;
    assert_plan_error(
        plan_for(&duplicate_fixture, "Sheet1", options(1)),
        |error| matches!(error, HeaderPlanError::DuplicatePath(_)),
    );
    Ok(())
}

#[test]
fn binding_requires_a_complete_one_to_one_field_set() -> anyhow::Result<()> {
    let rows = row(1, &format!("{}{}", inline("A1", "A"), inline("B1", "B")));
    let xml = worksheet("A1:B1", &rows, &[]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let plan = plan_for(&fixture, "Sheet1", options(1))?;
    let first = plan.columns()[0].path.clone();
    let second = plan.columns()[1].path.clone();

    let missing = plan.bind_fields(&[("field_a".into(), first.clone())]);
    let missing_error = missing.err().expect("missing a column binding must fail");
    assert!(matches!(
        missing_error.downcast_ref::<HeaderPlanError>(),
        Some(HeaderPlanError::BindingMismatch { .. })
    ));

    let duplicate_path = plan.bind_fields(&[
        ("field_a".into(), first.clone()),
        ("field_b".into(), first.clone()),
    ]);
    let duplicate_path_error = duplicate_path.err().expect("a repeated path must fail");
    assert!(matches!(
        duplicate_path_error.downcast_ref::<HeaderPlanError>(),
        Some(HeaderPlanError::InvalidBinding)
    ));

    let duplicate_name =
        plan.bind_fields(&[("same_name".into(), first), ("same_name".into(), second)]);
    let duplicate_name_error = duplicate_name
        .err()
        .expect("a repeated field name must fail");
    assert!(matches!(
        duplicate_name_error.downcast_ref::<HeaderPlanError>(),
        Some(HeaderPlanError::InvalidBinding)
    ));
    Ok(())
}

#[test]
fn plan_is_bound_to_workbook_and_rejects_sheet_switching() -> anyhow::Result<()> {
    let sheet1 = worksheet(
        "A1:B3",
        &format!(
            "{}{}{}",
            row(
                1,
                &format!("{}{}", inline("A1", "Id"), inline("B1", "Name"))
            ),
            row(2, &format!("{}{}", number("A2", 7), inline("B2", "first"))),
            row(3, &format!("{}{}", number("A3", 8), inline("B3", "second"))),
        ),
        &[],
    );
    let sheet2 = worksheet(
        "A1:A2",
        &format!(
            "{}{}",
            row(1, &inline("A1", "Other")),
            row(2, &number("A2", 9))
        ),
        &[],
    );
    let fixture = XlsxFixture::new(&[("Sheet1", &sheet1), ("Sheet2", &sheet2)])?;
    let workbook = fixture.open()?;
    let plan = inspect_header_plan(Arc::clone(&workbook), Some("Sheet1"), None, options(1))?;
    let id_path = plan.columns()[0].path.clone();
    let name_path = plan.columns()[1].path.clone();
    let bound = plan.bind_fields(&[("id".into(), id_path), ("name".into(), name_path)])?;
    let schema = schema([("name", DataType::String), ("id", DataType::Int64)]);

    let separately_opened = fixture.open()?;
    let wrong_workbook = StrictDataFrameIter::from_workbook_with_header_plan(
        Some(1),
        separately_opened,
        &bound,
        Arc::clone(&schema),
        None,
        false,
        None,
    )
    .err()
    .expect("plan from another workbook instance must fail");
    assert!(matches!(
        wrong_workbook.downcast_ref::<HeaderPlanError>(),
        Some(HeaderPlanError::WrongWorkbook)
    ));

    for fast in [false, true] {
        let mut iter = StrictDataFrameIter::from_workbook_with_header_plan(
            Some(1),
            Arc::clone(&workbook),
            &bound,
            Arc::clone(&schema),
            None,
            fast,
            None,
        )?;
        let select_error = iter
            .select_sheet(Some("Sheet2"), None)
            .expect_err("a planned iterator cannot change sheet");
        assert!(matches!(
            select_error.downcast_ref::<HeaderPlanError>(),
            Some(HeaderPlanError::CannotSelectSheetWithPlan)
        ));

        let frames = iter.collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert_eq!(
                frame
                    .get_column_names()
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>(),
                vec!["name", "id"]
            );
        }
        assert_eq!(frames[0].column("name")?.str()?.get(0), Some("first"));
        assert_eq!(frames[0].column("id")?.i64()?.get(0), Some(7));
        assert_eq!(frames[1].column("name")?.str()?.get(0), Some("second"));
        assert_eq!(frames[1].column("id")?.i64()?.get(0), Some(8));
    }
    Ok(())
}

#[test]
fn malformed_xml_after_sheet_data_is_reported() -> anyhow::Result<()> {
    let xml = "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:A1\"/><sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Header</t></is></c></row></sheetData><broken";
    let fixture = XlsxFixture::new(&[("Sheet1", xml)])?;
    let result = inspect_header_plan(fixture.open()?, Some("Sheet1"), None, options(1));
    assert!(
        result.is_err(),
        "malformed trailing XML must not be accepted"
    );
    Ok(())
}

#[test]
fn rejects_worksheet_root_missing_its_closing_tag() -> anyhow::Result<()> {
    let prefix = "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:A1\"/><sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Header</t></is></c></row></sheetData>";
    let fixture = XlsxFixture::new(&[("Sheet1", prefix)])?;
    let result = inspect_header_plan(fixture.open()?, Some("Sheet1"), None, options(1));
    assert!(result.is_err(), "缺少 </worksheet> 时不得返回表头计划");
    Ok(())
}

#[test]
fn rejects_a_second_root_after_the_closed_worksheet() -> anyhow::Result<()> {
    let xml = "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><dimension ref=\"A1:A1\"/><sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Header</t></is></c></row></sheetData></worksheet><extra/>";
    let fixture = XlsxFixture::new(&[("Sheet1", xml)])?;
    let result = inspect_header_plan(fixture.open()?, Some("Sheet1"), None, options(1));
    assert!(result.is_err(), "第二个根元素不得被接受为有效工作表");
    Ok(())
}

#[test]
fn two_level_plan_reads_data_after_skipped_notes_in_fast_and_stream_modes() -> anyhow::Result<()> {
    let rows = format!(
        "{}{}{}{}{}",
        row(1, &inline("A1", "说明行")),
        row(2, &inline("A2", "客户")),
        row(
            3,
            &format!("{}{}", inline("A3", "编号"), inline("B3", "姓名"))
        ),
        row(4, &format!("{}{}", number("A4", 71), inline("B4", "Alice"))),
        row(5, &format!("{}{}", number("A5", 72), inline("B5", "Bob"))),
    );
    let xml = worksheet("A1:B5", &rows, &["A2:B2"]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let workbook = fixture.open()?;
    let plan = inspect_header_plan(
        Arc::clone(&workbook),
        Some("Sheet1"),
        None,
        HeaderInspectOptions {
            skip_top_rows: 1,
            header_depth: 2,
            ..HeaderInspectOptions::default()
        },
    )?;
    assert_eq!(plan.first_header_row(), 1);
    assert_eq!(plan.data_start_row(), 3);
    let bound = plan.bind_fields(&[
        ("customer_id".into(), plan.columns()[0].path.clone()),
        ("customer_name".into(), plan.columns()[1].path.clone()),
    ])?;
    let accepted_schema = schema([
        ("customer_name", DataType::String),
        ("customer_id", DataType::Int64),
    ]);

    for fast in [false, true] {
        let frames = StrictDataFrameIter::from_workbook_with_header_plan(
            Some(1),
            Arc::clone(&workbook),
            &bound,
            Arc::clone(&accepted_schema),
            None,
            fast,
            None,
        )?
        .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(frames.len(), 2);
        for frame in &frames {
            assert_eq!(
                frame
                    .get_column_names()
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>(),
                vec!["customer_name", "customer_id"]
            );
        }
        assert_eq!(
            frames[0].column("customer_name")?.str()?.get(0),
            Some("Alice")
        );
        assert_eq!(frames[0].column("customer_id")?.i64()?.get(0), Some(71));
        assert_eq!(
            frames[1].column("customer_name")?.str()?.get(0),
            Some("Bob")
        );
        assert_eq!(frames[1].column("customer_id")?.i64()?.get(0), Some(72));
    }
    Ok(())
}

#[test]
fn unknown_right_edge_requires_explicit_count_when_deepest_header_and_data_are_blank()
-> anyhow::Result<()> {
    let rows = format!(
        "{}{}{}",
        row(1, &inline("A1", "标题")),
        row(2, &styled_blank("A2")),
        row(3, &styled_blank("A3")),
    );
    let xml = worksheet("A1:A3", &rows, &["A1:A2"]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;

    assert_plan_error(plan_for(&fixture, "Sheet1", options(2)), |error| {
        matches!(error, HeaderPlanError::UnknownRightBoundary)
    });

    let plan = plan_for(
        &fixture,
        "Sheet1",
        HeaderInspectOptions {
            accepted_column_count: Some(1),
            ..options(2)
        },
    )?;
    assert_eq!(plan.columns().len(), 1);
    assert_eq!(plan.columns()[0].path, XlsxHeaderPath(vec!["标题".into()]));
    Ok(())
}

#[test]
fn format_only_xfd_cells_do_not_expand_header_or_inspected_columns_but_real_data_fails()
-> anyhow::Result<()> {
    let rows = format!(
        "{}{}{}",
        row(1, &inline("A1", "Value")),
        row(2, &format!("{}{}", number("A2", 10), styled_blank("XFD2"))),
        row(3, &format!("{}{}", number("A3", 11), styled_blank("XFD3"))),
    );
    let xml = worksheet("A1:XFD3", &rows, &[]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let workbook = fixture.open()?;
    let plan = inspect_header_plan(Arc::clone(&workbook), Some("Sheet1"), None, options(1))?;
    assert_eq!(plan.columns().len(), 1);
    let bound = plan.bind_fields(&[("value".into(), plan.columns()[0].path.clone())])?;
    let accepted_schema = schema([("value", DataType::Int64)]);

    let inspection = inspect_sheet_schema_with_header_plan(Arc::clone(&workbook), &bound, None)?;
    assert_eq!(inspection.row_count, 2);
    assert_eq!(inspection.fields.len(), 1);
    assert_eq!(inspection.fields[0].name, "value");
    assert_eq!(inspection.fields[0].non_null_count, 2);

    for fast in [false, true] {
        let frames = StrictDataFrameIter::from_workbook_with_header_plan(
            Some(1),
            Arc::clone(&workbook),
            &bound,
            Arc::clone(&accepted_schema),
            None,
            fast,
            None,
        )?
        .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|frame| frame.width() == 1));
    }

    let extra_data_rows = format!(
        "{}{}",
        row(1, &inline("A1", "Value")),
        row(2, &format!("{}{}", number("A2", 12), number("XFD2", 99))),
    );
    let extra_data_xml = worksheet("A1:XFD2", &extra_data_rows, &[]);
    let extra_data_fixture = XlsxFixture::new(&[("Sheet1", &extra_data_xml)])?;
    let extra_workbook = extra_data_fixture.open()?;
    let extra_plan = inspect_header_plan(
        Arc::clone(&extra_workbook),
        Some("Sheet1"),
        None,
        options(1),
    )?;
    assert_eq!(extra_plan.columns().len(), 1);
    let extra_bound =
        extra_plan.bind_fields(&[("value".into(), extra_plan.columns()[0].path.clone())])?;
    for fast in [false, true] {
        let mut iter = StrictDataFrameIter::from_workbook_with_header_plan(
            Some(1),
            Arc::clone(&extra_workbook),
            &extra_bound,
            Arc::clone(&accepted_schema),
            None,
            fast,
            None,
        )?;
        let error = iter
            .next()
            .expect("non-empty data should produce a batch")
            .expect_err("real data outside the planned header must fail");
        assert!(matches!(
            error
                .downcast_ref::<StrictReadError>()
                .map(|error| &error.kind),
            Some(StrictReadErrorKind::SchemaMismatch(
                SchemaMismatch::ColumnOutsideHeader {
                    physical_column: 16_383,
                    header_column_count: 1,
                    ..
                }
            ))
        ));
    }
    Ok(())
}

#[test]
fn full_schema_inspection_uses_bound_names_and_counts_null_rows() -> anyhow::Result<()> {
    let rows = format!(
        "{}{}{}",
        row(
            1,
            &format!("{}{}", inline("A1", "姓名"), inline("B1", "编号"))
        ),
        row(2, &format!("{}{}", inline("A2", "Ada"), styled_blank("B2"))),
        row(3, &format!("{}{}", styled_blank("A3"), number("B3", 31))),
    );
    let xml = worksheet("A1:B3", &rows, &[]);
    let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
    let workbook = fixture.open()?;
    let plan = inspect_header_plan(Arc::clone(&workbook), Some("Sheet1"), None, options(1))?;
    let bound = plan.bind_fields(&[
        ("official_name".into(), plan.columns()[0].path.clone()),
        ("official_id".into(), plan.columns()[1].path.clone()),
    ])?;

    let inspection = inspect_sheet_schema_with_header_plan(workbook, &bound, None)?;
    assert_eq!(inspection.sheet, "Sheet1");
    assert_eq!(inspection.row_count, 2);
    assert_eq!(inspection.fields.len(), 2);
    assert_eq!(inspection.fields[0].name, "official_name");
    assert_eq!(inspection.fields[0].inferred_type, DataType::String);
    assert_eq!(
        inspection.fields[0].physical_kinds,
        [SheetPhysicalKind::Text].into()
    );
    assert_eq!(inspection.fields[0].non_null_count, 1);
    assert_eq!(inspection.fields[0].null_count, 1);
    assert_eq!(inspection.fields[1].name, "official_id");
    assert_eq!(inspection.fields[1].inferred_type, DataType::Int64);
    assert_eq!(inspection.fields[1].non_null_count, 1);
    assert_eq!(inspection.fields[1].null_count, 1);
    Ok(())
}

#[test]
fn malformed_merge_references_error_and_valid_lowercase_a1_remains_supported() -> anyhow::Result<()>
{
    assert_eq!(parse_a1(b"a1")?, (0, 0));
    assert_eq!(parse_a1(b"xfd1048576")?, (1_048_575, 16_383));
    for reference in ["B1:A1", "A0", "XFE1", "A1048577", "A1:B1garbage"] {
        let xml = worksheet(
            "A1:A2",
            &format!(
                "{}{}",
                row(1, &inline("A1", "Header")),
                row(2, &number("A2", 1))
            ),
            &[reference],
        );
        let fixture = XlsxFixture::new(&[("Sheet1", &xml)])?;
        let error = plan_for(&fixture, "Sheet1", options(1))
            .err()
            .expect("invalid mergeCell ref must fail");
        assert!(
            matches!(
                error.downcast_ref::<HeaderPlanError>(),
                Some(HeaderPlanError::InvalidMergeReference(_))
            ),
            "reference {reference:?}: {error:#}"
        );
    }
    for reference in [b"XFE1".as_slice(), b"A1048577", b"A0", b"A1garbage"] {
        assert!(parse_a1(reference).is_err(), "{reference:?}");
    }
    Ok(())
}
