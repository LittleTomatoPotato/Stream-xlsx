pub mod df_iter;
pub use df_iter::{
    DataFrameIter, SchemaMismatch, SheetFieldInspection, SheetPhysicalKind, SheetSchemaInspection,
    StrictDataFrameIter, StrictNumericReadAdapter, StrictNumericReadCell, StrictNumericReadValue,
    StrictReadError, StrictReadErrorKind, StrictReadOptions, df_iter, df_iter_fast,
    df_iter_fast_with_schema, df_iter_fast_with_schema_and_options, df_iter_with_schema,
    df_iter_with_schema_and_options, inspect_sheet_schema_from_workbook,
    inspect_sheet_schema_with_header_plan,
};
pub mod excel_types;
pub mod header;
pub use header::{
    BoundHeaderPlan, HeaderColumn, HeaderInspectOptions, HeaderMergeOverflow, HeaderMergeRange,
    HeaderPlanError, HeaderWarning, XlsxHeaderPath, XlsxHeaderPlan, inspect_header_plan,
};
pub mod sheet_fast;
pub use sheet_fast::FastConfig;
pub mod stream_reader;
pub mod utils;
pub mod workbook;
pub mod xlsx_stream_lm;

#[cfg(test)]
mod decompress_bench;
