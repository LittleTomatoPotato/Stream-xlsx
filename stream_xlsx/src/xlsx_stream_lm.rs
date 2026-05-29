/// 默认采用256KB作为内存切片的默认值, 后续可以考虑添加一个新的后台线程读取前N的块,记录最大值 暂时不做过度优化自动处理切片大小节省内存
use crate::excel_types::{Cell, Data, Dimensions};
use crate::utils::*;
use crate::workbook::XlsxWorkbook;
use anyhow::{Result, anyhow};
use bytes::{Bytes, BytesMut};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Arc;

const CHUNK_SIZE: usize = 256 * 1024; // 256K
const CHANNEL_CAPACITY: usize = 4;

/// 通过 channel 把后台线程的解压数据流式喂给前端 Reader。
/// 直接实现 BufRead，省去外层的 BufReader 包裹。
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Bytes>,
    current: Bytes,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: std::sync::mpsc::Receiver<Bytes>) -> Self {
        Self {
            rx,
            current: Bytes::new(),
            pos: 0,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut nread = 0;
        while nread < buf.len() {
            let slice = self.fill_buf()?;
            if slice.is_empty() {
                break;
            }
            let n = slice.len().min(buf.len() - nread);
            buf[nread..nread + n].copy_from_slice(&slice[..n]);
            nread += n;
            self.consume(n);
        }
        Ok(nread)
    }
}

impl BufRead for ChannelReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(data) => {
                    self.current = data;
                    self.pos = 0;
                }
                Err(_) => return Ok(&[]),
            }
        }
        Ok(&self.current[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.current.len());
    }
}

/// 独立的 xlsx sheet XML 流式读取器。
///
/// 不依赖 calamine，而是：
/// 1. 主线程先打开 ZIP 读取元数据（sharedStrings / workbook / rels），然后关闭。
/// 2. 启动后台线程，用**另一个** ZipArchive 实例逐块解压目标 sheet XML，
///    通过有限容量 channel 发送给主线程。
/// 3. 主线程用 quick-xml 从 channel 上逐事件解析 `<row>` / `<c>`。
pub struct XlsxStreamReader {
    xml: Reader<ChannelReader>,
    strings: Arc<Vec<Box<str>>>,
    cell_xfs: Arc<Vec<u32>>,
    custom_date_numfmts: Arc<HashSet<u32>>,
    date_columns: Vec<Option<bool>>,
    row_index: u32,
    col_index: u32,
    buf: Vec<u8>,
    cell_buf: Vec<u8>,
    dimensions: Dimensions,
    in_sheet_data: bool,
    scratch_buf: Vec<u8>,
}

impl XlsxStreamReader {
    /// 便捷方法：一次性打开文件并读取指定 sheet（向后兼容）。
    pub fn new<P: AsRef<Path>>(
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
    ) -> Result<Self> {
        let workbook = XlsxWorkbook::open(path)?;
        Self::from_workbook(Arc::new(workbook), sheet_name, sheet_idx)
    }

    /// 从已有的 workbook 创建 sheet 读取器（支持多 sheet 切换）。
    pub fn from_workbook(
        workbook: Arc<XlsxWorkbook>,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
    ) -> Result<Self> {
        let sheet_path = match (sheet_name, sheet_idx) {
            (Some(name), _) => workbook
                .sheet_path_by_name(name)
                .ok_or_else(|| anyhow!("Worksheet '{}' not found", name))?
                .to_string(),
            (None, Some(idx)) => workbook
                .sheet_path_by_idx(idx)
                .ok_or_else(|| anyhow!("Worksheet idx '{}' not found", idx))?
                .to_string(),
            (None, None) => workbook
                .sheet_path_by_idx(0)
                .ok_or_else(|| anyhow!("Worksheet idx '0' not found"))?
                .to_string(),
        };

        let path = workbook.path().to_owned();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Bytes>(CHANNEL_CAPACITY);
        std::thread::spawn(move || {
            let send_result = (|| -> Result<()> {
                let file = std::fs::File::open(&path)?;
                let reader = BufReader::new(file);
                let mut archive = zip::ZipArchive::new(reader)?;
                let mut zip_file = archive.by_name(&sheet_path)?;
                let mut accumulate = BytesMut::with_capacity(CHUNK_SIZE * 2);
                let mut temp = BytesMut::with_capacity(CHUNK_SIZE);
                loop {
                    temp.reserve(CHUNK_SIZE);
                    let spare = temp.spare_capacity_mut();
                    let dst = unsafe {
                        std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
                    };
                    match zip_file.read(dst) {
                        Ok(0) => {
                            if !accumulate.is_empty() {
                                if tx.send(accumulate.split().freeze()).is_err() {
                                    break;
                                }
                            }
                            break;
                        }
                        Ok(n) => {
                            unsafe {
                                temp.set_len(temp.len() + n);
                            }
                            accumulate.extend_from_slice(&temp[..n]);
                            temp.clear();
                            if accumulate.len() >= CHUNK_SIZE {
                                let chunk = accumulate.split_to(CHUNK_SIZE).freeze();
                                if tx.send(chunk).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = send_result {
                eprintln!("xlsx decompress thread error: {e}");
            }
        });

        let channel_reader = ChannelReader::new(rx);
        let mut xml = Reader::from_reader(channel_reader);
        let mut dimensions = Dimensions::default();
        let mut in_sheet_data = false;
        let mut pre_buf = Vec::with_capacity(1024);

        // 预读 XML 头部，解析 dimension，定位到 sheetData
        loop {
            pre_buf.clear();
            match xml.read_event_into(&mut pre_buf) {
                Ok(Event::Empty(e)) | Ok(Event::Start(e))
                    if e.local_name().as_ref() == b"dimension" =>
                {
                    if let Some(ref_attr) = get_attribute(&e, b"ref")? {
                        dimensions = parse_dimension(ref_attr.as_bytes())?;
                    }
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"sheetData" => {
                    in_sheet_data = true;
                    break;
                }
                Ok(Event::Empty(e)) if e.local_name().as_ref() == b"sheetData" => {
                    in_sheet_data = true;
                    break;
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow!("XML error: {}", e)),
                _ => {}
            }
        }

        let scratch_buf: Vec<u8> = Vec::new();

        Ok(Self {
            xml,
            strings: Arc::clone(workbook.strings()),
            cell_xfs: Arc::clone(workbook.cell_xfs()),
            custom_date_numfmts: Arc::clone(workbook.custom_date_numfmts()),
            date_columns: Vec::new(),
            row_index: 0,
            col_index: 0,
            buf: Vec::with_capacity(1024),
            cell_buf: Vec::with_capacity(1024),
            dimensions,
            in_sheet_data,
            scratch_buf,
        })
    }

    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    pub fn next_cell(&mut self) -> Result<Option<Cell<Data>>> {
        if !self.in_sheet_data {
            loop {
                self.buf.clear();
                match self.xml.read_event_into(&mut self.buf) {
                    Ok(Event::Start(e)) if e.local_name().as_ref() == b"sheetData" => {
                        self.in_sheet_data = true;
                        break;
                    }
                    Ok(Event::Empty(e)) if e.local_name().as_ref() == b"sheetData" => {
                        self.in_sheet_data = true;
                        return Ok(None);
                    }
                    Ok(Event::Eof) => return Ok(None),
                    Err(e) => return Err(anyhow!("XML error: {}", e)),
                    _ => {}
                }
            }
        }

        loop {
            self.buf.clear();
            let event = self.xml.read_event_into(&mut self.buf);
            let maybe_start = match event {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"row" => {
                    if let Some(r) = get_attribute(&e, b"r")? {
                        self.row_index = r.parse::<u32>()?.saturating_sub(1);
                    }
                    None
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"row" => {
                    self.row_index += 1;
                    self.col_index = 0;
                    None
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"c" => {
                    let mut pos = None;
                    let mut t_attr: Option<&str> = None;
                    let mut s_attr: Option<usize> = None;
                    for attr in e.attributes() {
                        let attr = attr?;
                        match attr.key.as_ref() {
                            b"r" => {
                                pos = Some(parse_a1(&attr.value)?);
                            }
                            b"t" => {
                                t_attr = match attr.value.as_ref() {
                                    b"s" => Some("s"),
                                    b"b" => Some("b"),
                                    b"e" => Some("e"),
                                    b"str" => Some("str"),
                                    b"d" => Some("d"),
                                    _ => None,
                                };
                            }
                            b"s" => {
                                s_attr = std::str::from_utf8(&attr.value)
                                    .ok()
                                    .and_then(|s| s.parse().ok());
                            }
                            _ => {}
                        }
                    }
                    let pos = pos.unwrap_or((self.row_index, self.col_index));
                    self.row_index = pos.0;
                    self.col_index = pos.1;
                    Some((pos, t_attr, s_attr, false))
                }
                Ok(Event::Empty(e)) if e.local_name().as_ref() == b"c" => {
                    let pos = parse_cell_pos(&e, self.row_index, self.col_index)?;
                    self.row_index = pos.0;
                    self.col_index = pos.1;
                    Some((pos, None, None, true))
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"sheetData" => {
                    self.in_sheet_data = false;
                    return Ok(None);
                }
                Ok(Event::Eof) => return Err(anyhow!("Unexpected EOF in sheetData")),
                Err(e) => return Err(anyhow!("XML error: {}", e)),
                _ => None,
            };

            if let Some((pos, t_attr, s_attr, is_empty)) = maybe_start {
                let value = if is_empty {
                    Data::Empty
                } else {
                    let s_attr_usize = s_attr;
                    let col_idx = pos.1 as usize;
                    if col_idx >= self.date_columns.len() {
                        self.date_columns.resize(col_idx + 1, None);
                    }
                    let is_date = match self.date_columns[col_idx] {
                        Some(cached) => cached,
                        None => {
                            let result = s_attr_usize
                                .and_then(|idx| self.cell_xfs.get(idx))
                                .map(|&id| self.is_date_numfmt(id))
                                .unwrap_or(false);
                            // 只在有 s 属性的单元格上缓存，避免无 s 属性的单元格（如 header）把列锁死
                            if s_attr_usize.is_some() {
                                self.date_columns[col_idx] = Some(result);
                            }
                            result
                        }
                    };
                    self.read_cell_value(t_attr, is_date)?
                };
                return Ok(Some(Cell::new(pos, value)));
            }
        }
    }

    fn is_date_numfmt(&self, num_fmt_id: u32) -> bool {
        crate::utils::is_date_numfmt(num_fmt_id, &self.custom_date_numfmts)
    }

    fn read_cell_value(&mut self, t_attr: Option<&str>, is_date: bool) -> Result<Data> {
        let mut value = Data::Empty;

        loop {
            self.cell_buf.clear();
            match self.xml.read_event_into(&mut self.cell_buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"v" => {
                    let text = crate::utils::read_text_content(
                        &mut self.xml,
                        &mut self.scratch_buf,
                        b"v",
                    )?;
                    value = parse_raw_value(&text, t_attr)?;
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"is" => {
                    let text = crate::utils::read_inline_str(
                        &mut self.xml,
                        &mut self.cell_buf,
                        &mut self.scratch_buf,
                    )?;
                    value = Data::String(text);
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"c" => break,
                Ok(Event::Eof) => return Err(anyhow!("Unexpected EOF in <c>")),
                Err(e) => return Err(anyhow!("XML error in <c>: {}", e)),
                _ => {}
            }
        }

        if let Some("s") = t_attr.as_deref() {
            if let Data::String(s) = &value {
                if let Ok(idx) = s.parse::<usize>() {
                    value = self
                        .strings
                        .get(idx)
                        .map(|s| Data::String(s.to_string()))
                        .unwrap_or(Data::Empty);
                }
            }
        }

        // 日期转换：已缓存为日期列的数字单元格直接转 DateTime
        if is_date && (t_attr.is_none() || t_attr.as_deref() == Some("")) {
            value = match value {
                Data::Int(v) => {
                    Data::DateTime(crate::excel_types::ExcelDateTime::new(v as f64, false))
                }
                Data::Float(v) => Data::DateTime(crate::excel_types::ExcelDateTime::new(v, false)),
                other => other,
            };
        }

        Ok(value)
    }
}

impl crate::stream_reader::StreamReader for XlsxStreamReader {
    fn dimensions(&self) -> crate::excel_types::Dimensions {
        self.dimensions()
    }
    fn next_cell(
        &mut self,
    ) -> anyhow::Result<Option<crate::excel_types::Cell<crate::excel_types::Data>>> {
        self.next_cell()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // #[global_allocator]
    // static ALLOC: dhat::Alloc = dhat::Alloc;
    #[test]
    fn test_stream_reader_unsafe() -> Result<()> {
        // let _profile = dhat::Profiler::new_heap();
        let start = std::time::Instant::now();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("test_data.xlsx");
        let mut reader = XlsxStreamReader::new(&path, "Sheet1".into(), None)?;
        let mut count = 0;
        println!("shape: {:?}", reader.dimensions().end);

        while let Some(cell) = reader.next_cell()? {
            if count < 14 {
                println!("{:?}: {:?}", cell.get_position(), cell.get_value());
            }
            count += 1;
            if count == reader.dimensions.end.0 + 1 {
                println!("last_cell :{:?}", cell.get_value());
            }
        }

        println!("total cells: {}", count);
        println!("elapsed: {:?}", start.elapsed());
        Ok(())
    }

    #[test]
    fn workbook_open_test() -> Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test_data.xlsx");
        println!("{:?}", path);
        let wb = XlsxWorkbook::open(&path)?;
        println!("sheet names: {:?}", wb.sheet_names());
        println!("sheet count: {}", wb.sheet_count());
        Ok(())
    }
}
