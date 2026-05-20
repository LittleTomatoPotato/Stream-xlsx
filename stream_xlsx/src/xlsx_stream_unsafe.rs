/// 默认采用256KB作为内存切片的默认值, 后续可以考虑添加一个新的后台线程读取前N的块,记录最大值 暂时不做过度优化自动处理切片大小节省内存
use crate::excel_types::{Cell, CellErrorType, Data, Dimensions};
use anyhow::{Context, Result, anyhow};
use bytes::{Bytes, BytesMut};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;
use zip::ZipArchive;

const CHUNK_SIZE: usize = 256 * 1024; // 256K
const CHANNEL_CAPACITY: usize = 4;

#[derive(Debug)]
struct OrderdSheets {
    order: Vec<(String, String)>,
    index: HashMap<String, usize>,
}
impl OrderdSheets {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            index: HashMap::new(),
        }
    }
    fn insert(&mut self, name: String, path: String) {
        let idx = self.order.len();
        self.index.insert(name.clone(), idx);
        self.order.push((name, path));
    }
    fn get_by_name(&self, name: &str) -> Option<&String> {
        self.index
            .get(name)
            .and_then(|&idx| self.order.get(idx))
            .map(|(_, path)| path)
    }
    fn get_by_idx(&self, idx: usize) -> Option<&String> {
        self.order.get(idx).map(|(_, path)| path)
    }
    // fn iter(&self) -> impl IntoIterator<Item = &(String, String)> {
    //     self.order.iter()
    // }
}

/// 通过 channel 把后台线程的解压数据流式喂给前端 Reader。
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Bytes>,
    current: Option<std::io::Cursor<Bytes>>,
}

impl ChannelReader {
    fn new(rx: std::sync::mpsc::Receiver<Bytes>) -> Self {
        Self { rx, current: None }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if let Some(ref mut cur) = self.current {
                let n = cur.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.current = None;
            }
            match self.rx.recv() {
                Ok(data) => self.current = Some(std::io::Cursor::new(data)),
                Err(_) => return Ok(0),
            }
        }
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
    xml: Reader<BufReader<ChannelReader>>,
    strings: Vec<String>,
    cell_xfs: Vec<u32>,
    custom_date_numfmts: HashSet<u32>,
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
    pub fn new<P: AsRef<Path>>(
        path: P,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
    ) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let (strings, sheet_path, cell_xfs, custom_date_numfmts) =
            Self::prepare(&path, sheet_name, sheet_idx)?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<Bytes>(CHANNEL_CAPACITY);
        std::thread::spawn(move || {
            let send_result = (|| -> Result<()> {
                let file = std::fs::File::open(&path)?;
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader)?;
                let mut zip_file = archive.by_name(&sheet_path)?;
                let mut buf = BytesMut::with_capacity(CHUNK_SIZE);
                // let mut sizes: Vec<usize> = Vec::new();
                loop {
                    buf.reserve(CHUNK_SIZE);
                    let spare = buf.spare_capacity_mut();
                    let dst = unsafe {
                        std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
                    };
                    match zip_file.read(dst) {
                        Ok(0) => break,
                        Ok(n) => {
                            // sizes.push(n);
                            unsafe {
                                buf.set_len(buf.len() + n);
                            }
                            let chunk = buf.split_to(n).freeze();
                            if tx.send(chunk).is_err() {
                                break;
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
        let mut xml = Reader::from_reader(BufReader::new(channel_reader));
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
                    if let Some(ref_attr) = Self::get_attribute(&e, b"ref")? {
                        dimensions = Self::parse_dimension(&ref_attr)?;
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
            strings,
            cell_xfs,
            custom_date_numfmts,
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
                    if let Some(r) = Self::get_attribute(&e, b"r")? {
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
                                if let Ok(r_str) = std::str::from_utf8(&attr.value) {
                                    pos = Some(Self::parse_a1(r_str)?);
                                }
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
                    let pos = Self::parse_cell_pos(&e, self.row_index, self.col_index)?;
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
                            self.date_columns[col_idx] = Some(result);
                            result
                        }
                    };
                    self.read_cell_value(t_attr, is_date)?
                };
                return Ok(Some(Cell::new(pos, value)));
            }
        }
    }

    fn prepare(
        path: &Path,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
    ) -> Result<(Vec<String>, String, Vec<u32>, HashSet<u32>)> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        let rels = Self::read_rels(&mut archive)?;
        let sheet_targets = Self::read_workbook_sheets(&mut archive, &rels)?;
        let sheet_path = match (sheet_name, sheet_idx) {
            (Some(name), _) => sheet_targets
                .get_by_name(name)
                .ok_or_else(|| anyhow!("Worksheet '{}' not found", name))?
                .clone(),
            (None, Some(idx)) => sheet_targets
                .get_by_idx(idx)
                .ok_or_else(|| anyhow!("Worksheet idx '{}' not found", idx))?
                .clone(),
            (None, None) => sheet_targets
                .get_by_idx(0)
                .ok_or_else(|| anyhow!("Worksheet idx '0' not found"))?
                .clone(),
        };
        let strings = Self::read_shared_strings(&mut archive)?;
        let (cell_xfs, custom_date_numfmts) = Self::read_styles(&mut archive)?;

        Ok((strings, sheet_path, cell_xfs, custom_date_numfmts))
    }

    fn read_rels<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<HashMap<String, String>> {
        let mut rels = HashMap::new();
        let mut file = match archive.by_name("xl/_rels/workbook.xml.rels") {
            Ok(f) => f,
            Err(_) => return Ok(rels),
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        drop(file);

        let mut reader = Reader::from_reader(Cursor::new(data));
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_event_into(&mut buf) {
                Ok(Event::Empty(e)) | Ok(Event::Start(e))
                    if e.local_name().as_ref() == b"Relationship" =>
                {
                    let mut id = String::new();
                    let mut target = String::new();
                    for attr in e.attributes() {
                        let attr = attr?;
                        match attr.key.as_ref() {
                            b"Id" => id = String::from_utf8_lossy(&attr.value).into_owned(),
                            b"Target" => target = String::from_utf8_lossy(&attr.value).into_owned(),
                            _ => {}
                        }
                    }
                    if !id.is_empty() {
                        let path = if target.starts_with('/') {
                            target[1..].to_string()
                        } else {
                            format!("xl/{}", target)
                        };
                        rels.insert(id, path);
                    }
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"Relationships" => break,
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow!("XML error in rels: {}", e)),
                _ => {}
            }
        }
        Ok(rels)
    }

    fn read_workbook_sheets<R: Read + Seek>(
        archive: &mut ZipArchive<R>,
        rels: &HashMap<String, String>,
    ) -> Result<OrderdSheets> {
        let mut file = archive
            .by_name("xl/workbook.xml")
            .context("workbook.xml not found")?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        drop(file);

        let mut reader = Reader::from_reader(Cursor::new(data));
        let mut buf = Vec::new();
        let mut sheets = OrderdSheets::new();

        loop {
            buf.clear();
            match reader.read_event_into(&mut buf) {
                Ok(Event::Empty(e)) | Ok(Event::Start(e))
                    if e.local_name().as_ref() == b"sheet" =>
                {
                    let mut name = String::new();
                    let mut id = String::new();
                    for attr in e.attributes() {
                        let attr = attr?;
                        match attr.key.local_name().as_ref() {
                            b"name" => {
                                name = reader.decoder().decode(&attr.value)?.into_owned();
                            }
                            b"id" => {
                                id = String::from_utf8_lossy(&attr.value).into_owned();
                            }
                            _ => {}
                        }
                    }
                    if let Some(path) = rels.get(&id) {
                        sheets.insert(name, path.clone());
                    }
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"workbook" => break,
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow!("XML error in workbook: {}", e)),
                _ => {}
            }
        }
        Ok(sheets)
    }

    fn read_shared_strings<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<Vec<String>> {
        let mut file = match archive.by_name("xl/sharedStrings.xml") {
            Ok(f) => f,
            Err(_) => return Ok(Vec::new()),
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        drop(file);

        let mut strings = Vec::new();
        let mut reader = Reader::from_reader(Cursor::new(data));
        let mut buf = Vec::new();
        let mut in_si = false;
        let mut current_text = String::new();

        loop {
            buf.clear();
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"si" => {
                    in_si = true;
                    current_text.clear();
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"si" => {
                    in_si = false;
                    strings.push(current_text.clone());
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"t" && in_si => {
                    let mut text_buf = Vec::new();
                    loop {
                        text_buf.clear();
                        match reader.read_event_into(&mut text_buf) {
                            Ok(Event::Text(t)) => {
                                current_text.push_str(&t.xml10_content().unwrap_or_default());
                            }
                            Ok(Event::CData(t)) => {
                                current_text.push_str(&String::from_utf8_lossy(t.as_ref()));
                            }
                            Ok(Event::End(e)) if e.local_name().as_ref() == b"t" => break,
                            Ok(Event::Eof) => {
                                return Err(anyhow!("Unexpected EOF in shared string"));
                            }
                            Err(e) => return Err(anyhow!("XML error in shared string: {}", e)),
                            _ => {}
                        }
                    }
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"sst" => break,
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow!("XML error in shared strings: {}", e)),
                _ => {}
            }
        }
        Ok(strings)
    }

    fn read_styles<R: Read + Seek>(
        archive: &mut ZipArchive<R>,
    ) -> Result<(Vec<u32>, HashSet<u32>)> {
        let mut file = match archive.by_name("xl/styles.xml") {
            Ok(f) => f,
            Err(_) => return Ok((Vec::new(), HashSet::new())),
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        drop(file);

        let mut reader = Reader::from_reader(Cursor::new(data));
        let mut buf = Vec::new();
        let mut cell_xfs: Vec<u32> = Vec::new();
        let mut custom_date_numfmts: HashSet<u32> = HashSet::new();
        let mut in_cell_xfs = false;
        let mut in_num_fmts = false;

        loop {
            buf.clear();
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"numFmts" => {
                    in_num_fmts = true;
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"numFmts" => {
                    in_num_fmts = false;
                }
                Ok(Event::Empty(e)) | Ok(Event::Start(e))
                    if in_num_fmts && e.local_name().as_ref() == b"numFmt" =>
                {
                    if let Some(id_str) = Self::get_attribute(&e, b"numFmtId")? {
                        if let Ok(id) = id_str.parse::<u32>() {
                            if let Some(code) = Self::get_attribute(&e, b"formatCode")? {
                                if Self::is_date_format_code(&code) {
                                    custom_date_numfmts.insert(id);
                                }
                            }
                        }
                    }
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"cellXfs" => {
                    in_cell_xfs = true;
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"cellXfs" => {
                    in_cell_xfs = false;
                }
                Ok(Event::Empty(e)) | Ok(Event::Start(e))
                    if in_cell_xfs && e.local_name().as_ref() == b"xf" =>
                {
                    let num_fmt_id = Self::get_attribute(&e, b"numFmtId")?
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    cell_xfs.push(num_fmt_id);
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"styleSheet" => break,
                Ok(Event::Eof) => break,
                Err(e) => return Err(anyhow!("XML error in styles: {}", e)),
                _ => {}
            }
        }
        Ok((cell_xfs, custom_date_numfmts))
    }

    fn is_date_format_code(code: &str) -> bool {
        let code_lower = code.to_lowercase();
        let date_keywords = ['y', 'm', 'd', 'h', 's'];
        date_keywords.iter().any(|&k| code_lower.contains(k))
            && !code_lower.contains("0")
            && !code_lower.contains("#")
    }

    fn is_date_numfmt(&self, num_fmt_id: u32) -> bool {
        matches!(num_fmt_id, 14..=22 | 45..=47) || self.custom_date_numfmts.contains(&num_fmt_id)
    }

    fn get_attribute(e: &BytesStart, key: &[u8]) -> Result<Option<String>> {
        for attr in e.attributes() {
            let attr = attr?;
            if attr.key.as_ref() == key {
                return Ok(Some(String::from_utf8_lossy(&attr.value).into_owned()));
            }
        }
        Ok(None)
    }

    fn parse_a1(s: &str) -> Result<(u32, u32)> {
        let idx = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        let col_str = &s[..idx];
        let row_str = &s[idx..];
        let row = row_str.parse::<u32>()?.saturating_sub(1);
        let mut col = 0u32;
        for c in col_str.chars() {
            col = col * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
        }
        Ok((row, col.saturating_sub(1)))
    }

    fn parse_dimension(ref_attr: &str) -> Result<Dimensions> {
        let parts: Vec<&str> = ref_attr.split(':').collect();
        if parts.len() == 1 {
            let (row, col) = Self::parse_a1(parts[0])?;
            Ok(Dimensions {
                start: (row, col),
                end: (row, col),
            })
        } else if parts.len() == 2 {
            let (start_row, start_col) = Self::parse_a1(parts[0])?;
            let (end_row, end_col) = Self::parse_a1(parts[1])?;
            Ok(Dimensions {
                start: (start_row, start_col),
                end: (end_row, end_col),
            })
        } else {
            Err(anyhow!("Invalid dimension: {}", ref_attr))
        }
    }

    fn parse_cell_pos(e: &BytesStart, default_row: u32, default_col: u32) -> Result<(u32, u32)> {
        if let Some(r) = Self::get_attribute(e, b"r")? {
            Self::parse_a1(&r)
        } else {
            Ok((default_row, default_col))
        }
    }

    fn read_cell_value(&mut self, t_attr: Option<&str>, is_date: bool) -> Result<Data> {
        let mut value = Data::Empty;

        loop {
            self.cell_buf.clear();
            match self.xml.read_event_into(&mut self.cell_buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"v" => {
                    let text = self.read_text_content(b"v")?;
                    value = Self::parse_raw_value(&text, t_attr)?;
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"is" => {
                    let text = self.read_inline_str()?;
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
                        .cloned()
                        .map(Data::String)
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

    fn read_text_content(&mut self, end_tag: &[u8]) -> Result<String> {
        let mut text = String::new();
        loop {
            self.scratch_buf.clear();
            match self.xml.read_event_into(&mut self.scratch_buf) {
                Ok(Event::Text(t)) => {
                    text.push_str(&t.xml10_content().unwrap_or_default());
                }
                Ok(Event::CData(t)) => {
                    text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == end_tag => break,
                Ok(Event::Eof) => return Err(anyhow!("Unexpected EOF in text content")),
                Err(e) => return Err(anyhow!("XML error: {}", e)),
                _ => {}
            }
        }
        Ok(text)
    }

    fn read_inline_str(&mut self) -> Result<String> {
        let mut text = String::new();
        loop {
            self.cell_buf.clear();
            match self.xml.read_event_into(&mut self.cell_buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"t" => {
                    text.push_str(&self.read_text_content(b"t")?);
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"is" => break,
                Ok(Event::Eof) => return Err(anyhow!("Unexpected EOF in <is>")),
                Err(e) => return Err(anyhow!("XML error in <is>: {}", e)),
                _ => {}
            }
        }
        Ok(text)
    }

    fn parse_raw_value(text: &str, t_attr: Option<&str>) -> Result<Data> {
        match t_attr {
            Some("s") => Ok(Data::String(text.to_string())),
            Some("b") => Ok(Data::Bool(text != "0")),
            Some("e") => {
                let err = text
                    .parse::<CellErrorType>()
                    .unwrap_or(CellErrorType::Value);
                Ok(Data::Error(err))
            }
            Some("str") => Ok(Data::String(text.to_string())),
            Some("d") => Ok(Data::DateTimeIso(text.to_string())),
            _ => {
                if let Ok(v) = text.parse::<i64>() {
                    Ok(Data::Int(v))
                } else if let Ok(v) = text.parse::<f64>() {
                    Ok(Data::Float(v))
                } else {
                    Ok(Data::String(text.to_string()))
                }
            }
        }
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
    fn prepare_test() -> Result<()> {
        fn read_rels<R: Read + Seek>(
            archive: &mut ZipArchive<R>,
        ) -> Result<HashMap<String, String>> {
            let mut rels = HashMap::new();
            let mut file = match archive.by_name("xl/_rels/workbook.xml.rels") {
                Ok(f) => f,
                Err(_) => return Ok(rels),
            };
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            drop(file);

            let mut reader = Reader::from_reader(Cursor::new(data));
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_event_into(&mut buf) {
                    Ok(Event::Empty(e)) | Ok(Event::Start(e))
                        if e.local_name().as_ref() == b"Relationship" =>
                    {
                        let mut id = String::new();
                        let mut target = String::new();
                        for attr in e.attributes() {
                            let attr = attr?;
                            match attr.key.as_ref() {
                                b"Id" => id = String::from_utf8_lossy(&attr.value).into_owned(),
                                b"Target" => {
                                    target = String::from_utf8_lossy(&attr.value).into_owned()
                                }
                                _ => {}
                            }
                        }
                        if !id.is_empty() {
                            let path = if target.starts_with('/') {
                                target[1..].to_string()
                            } else {
                                format!("xl/{}", target)
                            };
                            rels.insert(id, path);
                        }
                    }
                    Ok(Event::End(e)) if e.local_name().as_ref() == b"Relationships" => break,
                    Ok(Event::Eof) => break,
                    Err(e) => return Err(anyhow!("XML error in rels: {}", e)),
                    _ => {}
                }
            }
            Ok(rels)
        }
        fn read_workbook_sheets<R: Read + Seek>(
            archive: &mut ZipArchive<R>,
            rels: &HashMap<String, String>,
        ) -> Result<OrderdSheets> {
            let mut file = archive
                .by_name("xl/workbook.xml")
                .context("workbook.xml not found")?;
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            drop(file);

            let mut reader = Reader::from_reader(Cursor::new(data));
            let mut buf = Vec::new();
            let mut sheets = OrderdSheets::new();

            loop {
                buf.clear();
                match reader.read_event_into(&mut buf) {
                    Ok(Event::Empty(e)) | Ok(Event::Start(e))
                        if e.local_name().as_ref() == b"sheet" =>
                    {
                        let mut name = String::new();
                        let mut id = String::new();
                        for attr in e.attributes() {
                            let attr = attr?;
                            match attr.key.local_name().as_ref() {
                                b"name" => {
                                    name = reader.decoder().decode(&attr.value)?.into_owned();
                                }
                                b"id" => {
                                    id = String::from_utf8_lossy(&attr.value).into_owned();
                                }
                                _ => {}
                            }
                        }
                        if let Some(path) = rels.get(&id) {
                            sheets.insert(name, path.clone());
                        }
                    }
                    Ok(Event::End(e)) if e.local_name().as_ref() == b"workbook" => break,
                    Ok(Event::Eof) => break,
                    Err(e) => return Err(anyhow!("XML error in workbook: {}", e)),
                    _ => {}
                }
            }
            Ok(sheets)
        }

        let path = std::path::PathBuf::from("/Users/fc82/personal/project_x/test_data.xlsx");
        println!("{:?}", path);
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        let rels = read_rels(&mut archive)?;
        let sheet_targets = read_workbook_sheets(&mut archive, &rels)?;
        println!("{:?}", sheet_targets);
        Ok(())
    }
}
