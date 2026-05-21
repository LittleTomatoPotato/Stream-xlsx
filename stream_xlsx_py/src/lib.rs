use pyo3::prelude::*;
use pyo3_polars::PyDataFrame;
use stream_xlsx::df_iter::DataFrameIter;
use stream_xlsx::xlsx_stream_lm::XlsxStreamReader as XlsxStreamReaderLm;
use stream_xlsx::xlsx_stream_unsafe::XlsxStreamReader;

enum XlsxReaderInner {
    Default(DataFrameIter<XlsxStreamReader>),
    Lm(DataFrameIter<XlsxStreamReaderLm>),
}

/// Python 可迭代的流式 xlsx 读取器
///
/// 惰性逐批产生 DataFrame，不一次性载入内存。
#[pyclass(unsendable)]
pub struct XlsxReader {
    inner: XlsxReaderInner,
}

#[pymethods]
impl XlsxReader {
    /// 返回 self，使对象本身成为迭代器
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// 产生下一批 DataFrame；数据耗尽时自动触发 StopIteration
    fn __next__(&mut self) -> PyResult<Option<PyDataFrame>> {
        let result = match &mut self.inner {
            XlsxReaderInner::Default(iter) => iter.next(),
            XlsxReaderInner::Lm(iter) => iter.next(),
        };
        match result {
            Some(Ok(df)) => Ok(Some(PyDataFrame(df))),
            Some(Err(e)) => Err(pyo3::exceptions::PyRuntimeError::new_err(format!("{e}"))),
            None => Ok(None),
        }
    }

    /// 剩余批次数量（基于预计算的总批次数减去已产出数）
    fn __len__(&self) -> usize {
        match &self.inner {
            XlsxReaderInner::Default(iter) => iter.len(),
            XlsxReaderInner::Lm(iter) => iter.len(),
        }
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
/// - reader: 读取器类型，"default" 或 "lm"，默认 "default"
///
/// 用法:
/// ```python
/// for df in stream_xlsx.read_xlsx("data.xlsx", batch_size=1000, reader="lm"):
///     print(df.shape)
/// ```
#[pyfunction]
#[pyo3(signature = (path, batch_size=10000, sheet_name=None, sheet_idx=None, has_header=true, reader="default"))]
fn read_xlsx(
    path: &str,
    batch_size: Option<usize>,
    sheet_name: Option<String>,
    sheet_idx: Option<usize>,
    has_header: bool,
    reader: &str,
) -> PyResult<XlsxReader> {
    let sheet_name_ref = sheet_name.as_deref();
    let inner = match reader {
        "default" => {
            let iter = stream_xlsx::df_iter::df_iter::<XlsxStreamReader>(
                batch_size,
                path,
                sheet_name_ref,
                sheet_idx,
                has_header,
            )
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))?;
            XlsxReaderInner::Default(iter)
        }
        "lm" => {
            let iter = stream_xlsx::df_iter::df_iter::<XlsxStreamReaderLm>(
                batch_size,
                path,
                sheet_name_ref,
                sheet_idx,
                has_header,
            )
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("{e}")))?;
            XlsxReaderInner::Lm(iter)
        }
        _ => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "reader must be 'default' or 'lm'",
            ));
        }
    };
    Ok(XlsxReader { inner })
}

/// Python 模块初始化
#[pymodule]
fn stream_xlsx_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<XlsxReader>()?;
    m.add_function(wrap_pyfunction!(read_xlsx, m)?)?;
    Ok(())
}
