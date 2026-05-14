use crate::stream_xlsx::excel_types::{Cell, CellErrorType, Data, Dimensions};
use anyhow::{Context, Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;
use zip::ZipArchive;

const CHUNK_SIZE: usize = 64 * 1024;
const CHANNEL_CAPACITY: usize = 4;

/// 通过 channel 把后台线程的解压数据流式喂给前端 Reader。
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    current: Option<std::io::Cursor<Vec<u8>>>,
}

impl ChannelReader {
    fn new(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
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
    row_index: u32,
    col_index: u32,
    buf: Vec<u8>,
    cell_buf: Vec<u8>,
    dimensions: Dimensions,
    in_sheet_data: bool,
}

impl XlsxStreamReader {
    pub fn new<P: AsRef<Path>>(path: P, sheet_name: &str) -> Result<Self> {
        let path = path.as_ref().to_owned();

        let (strings, sheet_path) = Self::prepare(&path, sheet_name)?;

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(CHANNEL_CAPACITY);
        std::thread::spawn(move || {
            let send_result = (|| -> Result<()> {
                let file = std::fs::File::open(&path)?;
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader)?;
                let mut zip_file = archive.by_name(&sheet_path)?;
                let mut buf = vec![0u8; CHUNK_SIZE];
                loop {
                    match zip_file.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
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

        Ok(Self {
            xml,
            strings,
            row_index: 0,
            col_index: 0,
            buf: Vec::with_capacity(1024),
            cell_buf: Vec::with_capacity(1024),
            dimensions,
            in_sheet_data,
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
                    let pos = Self::parse_cell_pos(&e, self.row_index, self.col_index)?;
                    self.row_index = pos.0;
                    self.col_index = pos.1;
                    let t_attr = Self::get_attribute(&e, b"t")?;
                    Some((pos, t_attr, false))
                }
                Ok(Event::Empty(e)) if e.local_name().as_ref() == b"c" => {
                    let pos = Self::parse_cell_pos(&e, self.row_index, self.col_index)?;
                    self.row_index = pos.0;
                    self.col_index = pos.1;
                    Some((pos, None, true))
                }
                Ok(Event::End(e)) if e.local_name().as_ref() == b"sheetData" => {
                    return Ok(None);
                }
                Ok(Event::Eof) => return Err(anyhow!("Unexpected EOF in sheetData")),
                Err(e) => return Err(anyhow!("XML error: {}", e)),
                _ => None,
            };

            if let Some((pos, t_attr, is_empty)) = maybe_start {
                let value = if is_empty {
                    Data::Empty
                } else {
                    self.read_cell_value(t_attr)?
                };
                return Ok(Some(Cell::new(pos, value)));
            }
        }
    }

    fn prepare(path: &Path, sheet_name: &str) -> Result<(Vec<String>, String)> {
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        let rels = Self::read_rels(&mut archive)?;
        let sheet_targets = Self::read_workbook_sheets(&mut archive, &rels)?;
        let sheet_path = sheet_targets
            .get(sheet_name)
            .ok_or_else(|| anyhow!("Worksheet '{}' not found", sheet_name))?
            .clone();
        let strings = Self::read_shared_strings(&mut archive)?;

        Ok((strings, sheet_path))
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
    ) -> Result<HashMap<String, String>> {
        let mut file = archive
            .by_name("xl/workbook.xml")
            .context("workbook.xml not found")?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        drop(file);

        let mut reader = Reader::from_reader(Cursor::new(data));
        let mut buf = Vec::new();
        let mut sheets = HashMap::new();

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
        let mut col_str = String::new();
        let mut row_str = String::new();
        for c in s.chars() {
            if c.is_ascii_alphabetic() {
                col_str.push(c);
            } else {
                row_str.push(c);
            }
        }
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

    fn read_cell_value(&mut self, t_attr: Option<String>) -> Result<Data> {
        let mut value = Data::Empty;

        loop {
            self.cell_buf.clear();
            match self.xml.read_event_into(&mut self.cell_buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"v" => {
                    let text = self.read_text_content(b"v")?;
                    value = Self::parse_raw_value(&text, t_attr.as_deref())?;
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

        Ok(value)
    }

    fn read_text_content(&mut self, end_tag: &[u8]) -> Result<String> {
        let mut text = String::new();
        loop {
            let mut buf = Vec::new();
            match self.xml.read_event_into(&mut buf) {
                Ok(Event::Text(t)) => {
                    text.push_str(&t.xml10_content().unwrap_or_default());
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

    #[test]
    fn test_stream_reader() -> Result<()> {
        let mut reader = XlsxStreamReader::new("test_data.xlsx", "Sheet1")?;
        let mut count = 0;
        while let Some(cell) = reader.next_cell()? {
            if count < 5 {
                println!("{:?}: {:?}", cell.get_position(), cell.get_value());
            }
            count += 1;
        }
        println!("total cells: {}", count);
        Ok(())
    }
}
