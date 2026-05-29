use pyo3::prelude::*;
use pyo3_polars::PyDataFrame;
use std::sync::Arc;
use stream_xlsx::df_iter::DataFrameIter;
use stream_xlsx::workbook::XlsxWorkbook;

/// Python 可迭代的流式 xlsx 读取器
///
/// 惰性逐批产生 DataFrame，不一次性载入内存。
/// 支持多 sheet 切换：打开后可用 `sheet_names` 查看所有 sheet，
/// 用 `select_sheet` / `select_sheet_by_idx` 切换。
#[pyclass(unsendable)]
pub struct XlsxReader {
    workbook: Arc<XlsxWorkbook>,
    inner: DataFrameIter,
}

#[pymethods]
impl XlsxReader {
    /// 返回 self，使对象本身成为迭代器
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// 产生下一批 DataFrame；数据耗尽时自动触发 StopIteration
    fn __next__(&mut self) -> PyResult<Option<PyDataFrame>> {
        match self.inner.next() {
            Some(Ok(df)) => Ok(Some(PyDataFrame(df))),
            Some(Err(e)) => Err(pyo3::exceptions::PyRuntimeError::new_err(format!("{e}"))),
            None => Ok(None),
        }
    }

    /// 剩余批次数量（基于预计算的总批次数减去已产出数）
    fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// 返回所有 sheet 名称列表
    fn sheet_names(&self) -> Vec<String> {
        self.workbook
            .sheet_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// 按名称切换 sheet，重置迭代器状态
    fn select_sheet(&mut self, sheet_name: &str) -> PyResult<()> {
        self.inner
            .select_sheet(Some(sheet_name), None)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))
    }

    /// 按索引切换 sheet（0-based），重置迭代器状态
    fn select_sheet_by_idx(&mut self, sheet_idx: usize) -> PyResult<()> {
        self.inner
            .select_sheet(None, Some(sheet_idx))
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))
    }
}

/// 打开 xlsx 文件，返回一个惰性迭代器（Python generator 语义）
///
/// 参数:
/// - path: 文件路径
/// - batch_size: 每批读取的行数，默认 10000
/// - sheet_name: 工作表名称（可选）
/// - sheet_idx: 工作表索引（可选，0-based）
/// - has_header: 是否将第一行作为表头，默认 True
///
/// 用法:
/// ```python
/// reader = stream_xlsx.read_xlsx("data.xlsx", batch_size=1000)
/// for df in reader:
///     print(df.shape)
/// # 切换 sheet
/// reader.select_sheet("Sheet2")
/// for df in reader:
///     print(df.shape)
/// ```
#[pyfunction]
#[pyo3(signature = (path, batch_size=10000, sheet_name=None, sheet_idx=None, has_header=true, skip_rows=None))]
fn read_xlsx(
    path: &str,
    batch_size: Option<usize>,
    sheet_name: Option<String>,
    sheet_idx: Option<usize>,
    has_header: bool,
    skip_rows: Option<Vec<u32>>,
) -> PyResult<XlsxReader> {
    let workbook = Arc::new(
        XlsxWorkbook::open(path)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))?,
    );
    let sheet_name_ref = sheet_name.as_deref();
    let skip_rows_ref = skip_rows.as_deref();
    let iter = DataFrameIter::from_workbook(
        batch_size,
        Arc::clone(&workbook),
        sheet_name_ref,
        sheet_idx,
        has_header,
        skip_rows_ref,
    )
    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))?;
    Ok(XlsxReader {
        workbook,
        inner: iter,
    })
}

/// Python 模块初始化
#[pymodule]
fn stream_xlsx_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<XlsxReader>()?;
    m.add_function(wrap_pyfunction!(read_xlsx, m)?)?;
    Ok(())
}
