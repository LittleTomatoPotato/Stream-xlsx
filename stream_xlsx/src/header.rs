//! 多级合并表头的结构化检查与字段绑定。只保留表头，不缓存数据行。

use crate::{excel_types::Data, workbook::XlsxWorkbook, xlsx_stream_lm::XlsxStreamReader};
use anyhow::{Result, anyhow};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

pub(crate) const MAX_COLUMNS: u32 = 16_384;
const MAX_ROWS: u32 = 1_048_576;

/// 结构化标题路径。标签内的 `.` 等字符不具有路径分隔含义。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct XlsxHeaderPath(pub Vec<String>);

/// Excel 零起始物理坐标，起止位置均包含在内。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderMergeRange {
    pub start: (u32, u32),
    pub end: (u32, u32),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeaderMergeOverflow {
    #[default]
    Error,
    ClipAndWarn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderInspectOptions {
    pub skip_top_rows: u32,
    pub header_depth: u8,
    pub merge_overflow: HeaderMergeOverflow,
    /// 最深表头和数据区都没有非空列时，可由已确认的 Schema 提供列数。
    /// 首次建源没有 Schema 时必须为 None，不从合并标题或 dimension 猜测。
    pub accepted_column_count: Option<usize>,
}
impl Default for HeaderInspectOptions {
    fn default() -> Self {
        Self {
            skip_top_rows: 0,
            header_depth: 1,
            merge_overflow: HeaderMergeOverflow::Error,
            accepted_column_count: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderColumn {
    pub physical_column: u32,
    pub path: XlsxHeaderPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderWarning {
    MergeClipped {
        range: HeaderMergeRange,
        rightmost_column: u32,
    },
}

/// 表头检查的机读错误；不包含数据区原始值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderPlanError {
    InvalidOptions,
    MissingHeader,
    UnknownRightBoundary,
    InvalidMergeReference(String),
    MergeCrossesHeaderBoundary(HeaderMergeRange),
    MergeOverflow {
        range: HeaderMergeRange,
        rightmost_column: u32,
    },
    OverlappingMerges {
        row: u32,
        column: u32,
    },
    ConflictingMergedLabel {
        row: u32,
        column: u32,
    },
    InvalidCellOrder,
    InvalidCellCoordinate,
    DuplicatePath(XlsxHeaderPath),
    InvalidBinding,
    BindingMismatch {
        missing_fields: Vec<String>,
        extra_paths: Vec<XlsxHeaderPath>,
    },
    WrongWorkbook,
    CannotSelectSheetWithPlan,
}
impl fmt::Display for HeaderPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "XLSX 表头计划无效：{self:?}")
    }
}
impl std::error::Error for HeaderPlanError {}

/// 绑定到同一个已打开工作簿、一个 Sheet 的不可变表头证据。
/// 保存原始路径和实际合并范围，展示名不参与字段身份判断。
pub struct XlsxHeaderPlan {
    workbook: Arc<XlsxWorkbook>,
    sheet_name: String,
    first_header_row: u32,
    options: HeaderInspectOptions,
    columns: Vec<HeaderColumn>,
    merges: Vec<HeaderMergeRange>,
    warnings: Vec<HeaderWarning>,
}
impl XlsxHeaderPlan {
    pub fn sheet_name(&self) -> &str {
        &self.sheet_name
    }
    pub fn first_header_row(&self) -> u32 {
        self.first_header_row
    }
    pub fn data_start_row(&self) -> u32 {
        self.first_header_row + u32::from(self.options.header_depth)
    }
    pub fn options(&self) -> HeaderInspectOptions {
        self.options
    }
    pub fn columns(&self) -> &[HeaderColumn] {
        &self.columns
    }
    pub fn merges(&self) -> &[HeaderMergeRange] {
        &self.merges
    }
    pub fn warnings(&self) -> &[HeaderWarning] {
        &self.warnings
    }

    /// 显式绑定 AcceptedSchema 字段名和原始标题路径，必须恰好一一对应。
    /// 不拆分展示名，也不自动为重复路径追加后缀。
    pub fn bind_fields(
        self: &Arc<Self>,
        fields: &[(String, XlsxHeaderPath)],
    ) -> Result<BoundHeaderPlan> {
        let mut names = BTreeSet::new();
        let mut by_path = BTreeMap::new();
        for (name, path) in fields {
            if name.is_empty()
                || path.0.is_empty()
                || path.0.iter().any(String::is_empty)
                || !names.insert(name.as_str())
                || by_path.insert(path.clone(), name).is_some()
            {
                return Err(HeaderPlanError::InvalidBinding.into());
            }
        }
        let actual: BTreeSet<_> = self.columns.iter().map(|column| &column.path).collect();
        let missing_fields = fields
            .iter()
            .filter(|(_, path)| !actual.contains(path))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let extra_paths = self
            .columns
            .iter()
            .filter(|column| !by_path.contains_key(&column.path))
            .map(|column| column.path.clone())
            .collect::<Vec<_>>();
        if !missing_fields.is_empty() || !extra_paths.is_empty() {
            return Err(HeaderPlanError::BindingMismatch {
                missing_fields,
                extra_paths,
            }
            .into());
        }
        let field_names = self
            .columns
            .iter()
            .map(|column| by_path[&column.path].to_string())
            .collect();
        Ok(BoundHeaderPlan {
            plan: Arc::clone(self),
            field_names,
        })
    }
}

/// 已唯一绑定字段名的计划。构造入口只有 `XlsxHeaderPlan::bind_fields`。
#[derive(Clone)]
pub struct BoundHeaderPlan {
    plan: Arc<XlsxHeaderPlan>,
    field_names: Vec<String>,
}
impl BoundHeaderPlan {
    pub fn header_plan(&self) -> &Arc<XlsxHeaderPlan> {
        &self.plan
    }
    pub(crate) fn field_names(&self) -> &[String] {
        &self.field_names
    }
    pub(crate) fn validate_workbook(&self, workbook: &Arc<XlsxWorkbook>) -> Result<()> {
        if !Arc::ptr_eq(&self.plan.workbook, workbook) {
            return Err(HeaderPlanError::WrongWorkbook.into());
        }
        Ok(())
    }
}

/// 消费完整 Sheet XML 和 ZIP 尾部，只保存至多 8×16384 个表头单元格及合并信息。
/// 表头起点是跳过顶部行后的首个非空单元格；`dimension` 和格式空格不扩大右边界。
/// 当前绑定比较原始标签；名称归一化不能在此隐式放宽。
pub fn inspect_header_plan(
    workbook: Arc<XlsxWorkbook>,
    sheet_name: Option<&str>,
    sheet_idx: Option<usize>,
    options: HeaderInspectOptions,
) -> Result<Arc<XlsxHeaderPlan>> {
    if !(1..=8).contains(&options.header_depth)
        || options.skip_top_rows >= MAX_ROWS
        || options
            .accepted_column_count
            .is_some_and(|count| count == 0 || count > MAX_COLUMNS as usize)
    {
        return Err(HeaderPlanError::InvalidOptions.into());
    }
    let names = workbook.sheet_names();
    let name = match sheet_name {
        Some(name) if names.contains(&name) => name.to_owned(),
        Some(_) => return Err(anyhow!("工作表不存在")),
        None => names
            .get(sheet_idx.unwrap_or(0))
            .ok_or_else(|| anyhow!("工作表不存在"))?
            .to_string(),
    };
    let mut reader = XlsxStreamReader::from_workbook(Arc::clone(&workbook), Some(&name), None)?;
    let mut first_header_row = None;
    let mut labels = BTreeMap::new();
    let mut deepest_right = None;
    let mut data_right = None;
    let mut previous = None;
    while let Some(cell) = reader.next_cell()? {
        let position = cell.get_position();
        if position.0 >= MAX_ROWS || position.1 >= MAX_COLUMNS {
            return Err(HeaderPlanError::InvalidCellCoordinate.into());
        }
        if previous.is_some_and(|previous| previous >= position) {
            return Err(HeaderPlanError::InvalidCellOrder.into());
        }
        previous = Some(position);
        if position.0 < options.skip_top_rows {
            continue;
        }
        if !cell_has_value(cell.get_value(), reader.strings())? {
            continue;
        }
        let first_row = *first_header_row.get_or_insert(position.0);
        let data_start = first_row + u32::from(options.header_depth);
        if data_start > MAX_ROWS {
            return Err(HeaderPlanError::InvalidOptions.into());
        }
        reader.set_header_merge_rows(first_row, data_start - 1);
        if position.0 < data_start {
            if matches!(cell.get_value(), Data::Error(_)) {
                return Err(anyhow!("表头单元格包含 Excel 错误"));
            }
            let value =
                crate::df_iter::cell_value_to_header(cell.into_value(), Some(reader.strings()));
            if position.0 == data_start - 1 {
                deepest_right =
                    Some(deepest_right.map_or(position.1, |right: u32| right.max(position.1)));
            }
            labels.insert(position, value);
        } else {
            data_right = Some(data_right.map_or(position.1, |right: u32| right.max(position.1)));
        }
    }
    let first_header_row = first_header_row.ok_or(HeaderPlanError::MissingHeader)?;
    let right = deepest_right
        .or(data_right)
        .or_else(|| options.accepted_column_count.map(|count| count as u32 - 1))
        .ok_or(HeaderPlanError::UnknownRightBoundary)?;
    let merges = reader.take_header_merges();
    assemble_plan(
        workbook,
        name,
        first_header_row,
        options,
        labels,
        right,
        merges,
    )
}

fn cell_has_value(data: &Data, strings: &crate::workbook::SharedStrings) -> Result<bool> {
    Ok(match data {
        Data::Empty => false,
        Data::String(value) | Data::DateTimeIso(value) | Data::DurationIso(value) => {
            !value.is_empty()
        }
        Data::SharedStringRef(index) => {
            strings
                .offsets
                .get(*index)
                .ok_or_else(|| anyhow!("单元格引用了不存在的 shared string"))?
                .1
                > 0
        }
        _ => true,
    })
}

#[allow(clippy::too_many_arguments)]
fn assemble_plan(
    workbook: Arc<XlsxWorkbook>,
    sheet_name: String,
    first_header_row: u32,
    options: HeaderInspectOptions,
    labels: BTreeMap<(u32, u32), String>,
    right: u32,
    merges: Vec<HeaderMergeRange>,
) -> Result<Arc<XlsxHeaderPlan>> {
    let data_start = first_header_row + u32::from(options.header_depth);
    // 以 Excel 列上限而非声明合并范围分配，避免超大范围放大内存或循环次数。
    let mut owners = vec![None; MAX_COLUMNS as usize * usize::from(options.header_depth)];
    let mut warnings = Vec::new();
    for (index, range) in merges.iter().enumerate() {
        if range.start.0 < first_header_row || range.end.0 >= data_start {
            return Err(HeaderPlanError::MergeCrossesHeaderBoundary(*range).into());
        }
        if range.end.1 > right {
            if options.merge_overflow == HeaderMergeOverflow::Error {
                return Err(HeaderPlanError::MergeOverflow {
                    range: *range,
                    rightmost_column: right,
                }
                .into());
            }
            warnings.push(HeaderWarning::MergeClipped {
                range: *range,
                rightmost_column: right,
            });
        }
        for row in range.start.0..=range.end.0 {
            for column in range.start.1..=range.end.1 {
                let owner = &mut owners[((row - first_header_row) * MAX_COLUMNS + column) as usize];
                if owner.replace(index).is_some() {
                    return Err(HeaderPlanError::OverlappingMerges { row, column }.into());
                }
                if (row, column) != range.start && labels.contains_key(&(row, column)) {
                    return Err(HeaderPlanError::ConflictingMergedLabel { row, column }.into());
                }
            }
        }
    }
    let mut unique = BTreeSet::new();
    let mut columns = Vec::with_capacity(right as usize + 1);
    for column in 0..=right {
        let mut path = Vec::new();
        let mut emitted_merges = BTreeSet::new();
        for row in first_header_row..data_start {
            let owner = owners[((row - first_header_row) * MAX_COLUMNS + column) as usize];
            let anchor = match owner {
                Some(index) if emitted_merges.insert(index) => merges[index].start,
                Some(_) => continue,
                None => (row, column),
            };
            if let Some(label) = labels.get(&anchor) {
                path.push(label.clone());
            }
        }
        if path.is_empty() {
            path.push(format!("Unknown_{column}"));
        }
        let path = XlsxHeaderPath(path);
        if !unique.insert(path.clone()) {
            return Err(HeaderPlanError::DuplicatePath(path).into());
        }
        columns.push(HeaderColumn {
            physical_column: column,
            path,
        });
    }
    Ok(Arc::new(XlsxHeaderPlan {
        workbook,
        sheet_name,
        first_header_row,
        options,
        columns,
        merges,
        warnings,
    }))
}

/// 合并引用采用严格 Excel 范围语法与坐标上限，不使用宽松的旧 `parse_a1`。
pub(crate) fn parse_merge_reference(reference: &str) -> Result<HeaderMergeRange> {
    fn cell(reference: &str) -> Option<(u32, u32)> {
        let split = reference
            .bytes()
            .position(|byte| !byte.is_ascii_uppercase())?;
        if split == 0 || split == reference.len() {
            return None;
        }
        let mut column = 0_u32;
        for byte in reference[..split].bytes() {
            column = column
                .checked_mul(26)?
                .checked_add(u32::from(byte - b'A' + 1))?;
        }
        let digits = &reference[split..];
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) || digits.starts_with('0') {
            return None;
        }
        let row = digits.parse::<u32>().ok()?;
        if row == 0 || row > MAX_ROWS || column == 0 || column > MAX_COLUMNS {
            return None;
        }
        Some((row - 1, column - 1))
    }
    let (start, end) = reference.split_once(':').unwrap_or((reference, reference));
    match (cell(start), cell(end)) {
        (Some(start), Some(end)) if start.0 <= end.0 && start.1 <= end.1 => {
            Ok(HeaderMergeRange { start, end })
        }
        _ => Err(HeaderPlanError::InvalidMergeReference(reference.to_owned()).into()),
    }
}
