use crate::utils::*;
use anyhow::{Context, Result, anyhow};
use polars_buffer::Buffer;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use zip::ZipArchive;

/// 连续 buffer + offsets 形式的共享字符串表，可被 Arrow StringView 零拷贝引用。
#[derive(Debug)]
pub struct SharedStrings {
    pub buffer: Buffer<u8>,
    pub offsets: Vec<(u32, u32)>, // (offset, length)
}

/// 工作簿级共享数据，解析 sheet 列表立即完成，strings/styles 惰性加载。
#[derive(Debug)]
pub struct XlsxWorkbook {
    path: PathBuf,
    strings: OnceLock<Arc<SharedStrings>>,
    cell_xfs: OnceLock<Arc<Vec<u32>>>,
    custom_date_numfmts: OnceLock<Arc<HashSet<u32>>>,
    sheets: OrderdSheets,
    /// If true, decompress sharedStrings.xml fully into memory and parse with
    /// a byte scanner (~5× faster, ~+2-4GB peak memory).
    fast_shared_strings: bool,
}

impl XlsxWorkbook {
    /// Open workbook in low-memory mode (default).
    /// sharedStrings.xml is parsed streaming via quick-xml.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_mode(path, false)
    }

    /// Open workbook in fast mode.
    /// sharedStrings.xml is fully decompressed then byte-scanned.
    pub fn open_fast<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_mode(path, true)
    }

    fn open_with_mode<P: AsRef<Path>>(path: P, fast_shared_strings: bool) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        let rels = Self::read_rels(&mut archive)?;
        let sheets = Self::read_workbook_sheets(&mut archive, &rels)?;

        Ok(Self {
            path,
            strings: OnceLock::new(),
            cell_xfs: OnceLock::new(),
            custom_date_numfmts: OnceLock::new(),
            sheets,
            fast_shared_strings,
        })
    }

    /// 惰性加载 sharedStrings 和 styles。线程安全，只执行一次。
    pub fn init(&self) -> Result<()> {
        if self.strings.get().is_some() {
            return Ok(());
        }
        let file = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;

        let strings = Arc::new(Self::read_shared_strings(&mut archive, self.fast_shared_strings)?);
        let (cell_xfs, custom_date_numfmts) = Self::read_styles(&mut archive)?;

        let _ = self.strings.set(strings);
        let _ = self.cell_xfs.set(Arc::new(cell_xfs));
        let _ = self.custom_date_numfmts.set(Arc::new(custom_date_numfmts));
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn strings(&self) -> Option<&Arc<SharedStrings>> {
        self.strings.get()
    }

    pub fn cell_xfs(&self) -> Option<&Arc<Vec<u32>>> {
        self.cell_xfs.get()
    }

    pub fn custom_date_numfmts(&self) -> Option<&Arc<HashSet<u32>>> {
        self.custom_date_numfmts.get()
    }

    pub fn sheet_count(&self) -> usize {
        self.sheets.len()
    }

    pub fn sheet_names(&self) -> Vec<&str> {
        self.sheets.names()
    }

    pub fn sheet_path_by_name(&self, name: &str) -> Option<&str> {
        self.sheets.get_by_name(name).map(|s| s.as_str())
    }

    pub fn sheet_path_by_idx(&self, idx: usize) -> Option<&str> {
        self.sheets.get_by_idx(idx).map(|s| s.as_str())
    }

    // ------------------------------------------------------------------
    // internal helpers
    // ------------------------------------------------------------------

    fn read_rels<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<HashMap<String, String>> {
        let mut rels = HashMap::new();
        let file = match archive.by_name("xl/_rels/workbook.xml.rels") {
            Ok(f) => f,
            Err(_) => return Ok(rels),
        };
        let mut reader = Reader::from_reader(BufReader::with_capacity(64 * 1024, file));
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
        let file = archive
            .by_name("xl/workbook.xml")
            .context("workbook.xml not found")?;
        let mut reader = Reader::from_reader(BufReader::with_capacity(64 * 1024, file));
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

    fn read_shared_strings<R: Read + Seek>(
        archive: &mut ZipArchive<R>,
        fast: bool,
    ) -> Result<SharedStrings> {
        let file = match archive.by_name("xl/sharedStrings.xml") {
            Ok(f) => f,
            Err(_) => return Ok(SharedStrings {
                buffer: Buffer::from_vec(Vec::new()),
                offsets: Vec::new(),
            }),
        };

        if fast {
            // Aggressive mode: fully decompress then byte-scan.
            // Peak memory += ~2-4GB (uncompressed XML + buffers).
            let mut xml = Vec::with_capacity(file.size() as usize);
            BufReader::with_capacity(256 * 1024, file).read_to_end(&mut xml)?;

            if let Ok(result) = Self::parse_shared_strings_fast(&xml) {
                return Ok(result);
            }
            Self::parse_shared_strings_xml(&xml)
        } else {
            // Low-memory mode: stream via quick-xml (default).
            let mut buffer = Vec::with_capacity(64 * 1024);
            let mut offsets = Vec::new();
            let mut reader = Reader::from_reader(BufReader::with_capacity(256 * 1024, file));
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
                        let start = buffer.len() as u32;
                        let bytes = current_text.as_bytes();
                        buffer.extend_from_slice(bytes);
                        offsets.push((start, bytes.len() as u32));
                        current_text.clear();
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
            Ok(SharedStrings {
                buffer: Buffer::from_vec(buffer),
                offsets,
            })
        }
    }

    /// Fast path: scan raw bytes for `<si><t>...</t></si>` patterns.
    /// Falls back to XML parser on rich text (`<r>`), CDATA, or XML entities.
    fn parse_shared_strings_fast(xml: &[u8]) -> Result<SharedStrings> {
        let mut buffer = Vec::with_capacity(xml.len() / 4);
        let mut offsets = Vec::with_capacity(xml.len() / 32);

        let mut i = 0;
        while i < xml.len() {
            // Find <si>
            let si_pos = match find_subsequence(&xml[i..], b"<si>") {
                Some(p) => p,
                None => break,
            };
            i += si_pos + 4;

            // Find </si> to determine the boundary of this <si>
            let si_end = match find_subsequence(&xml[i..], b"</si>") {
                Some(p) => p,
                None => return Err(anyhow!("No </si> found")),
            };
            let si_content = &xml[i..i + si_end];

            // Check for rich text (<r>) or CDATA — not supported in fast path
            if si_content.windows(3).any(|w| w == b"<r>") {
                return Err(anyhow!("Rich text <r> not supported in fast path"));
            }
            if si_content.windows(9).any(|w| w == b"<![CDATA[") {
                return Err(anyhow!("CDATA not supported in fast path"));
            }

            // Find <t> or <t ...> within this <si>
            let t_pos = match find_subsequence(si_content, b"<t") {
                Some(p) => p,
                None => {
                    // Empty string: <si></si> or <si><t/></si>
                    offsets.push((buffer.len() as u32, 0));
                    i += si_end + 5;
                    continue;
                }
            };

            let mut t_start = t_pos + 2;
            // Skip attributes until '>'
            while t_start < si_content.len() && si_content[t_start] != b'>' {
                if si_content[t_start] == b'&' {
                    return Err(anyhow!("XML entity in <t> attribute not supported"));
                }
                t_start += 1;
            }
            t_start += 1; // skip '>'

            // Find </t>
            let t_end = match find_subsequence(&si_content[t_start..], b"</t>") {
                Some(p) => p,
                None => return Err(anyhow!("No </t> found")),
            };

            let text = &si_content[t_start..t_start + t_end];

            // XML entity check in text content
            if text.contains(&b'&') {
                return Err(anyhow!("XML entity in text not supported in fast path"));
            }

            let start = buffer.len() as u32;
            buffer.extend_from_slice(text);
            offsets.push((start, t_end as u32));

            i += si_end + 5;
        }

        Ok(SharedStrings {
            buffer: Buffer::from_vec(buffer),
            offsets,
        })
    }

    /// Fallback: full quick-xml parser for complex sharedStrings.
    fn parse_shared_strings_xml(xml: &[u8]) -> Result<SharedStrings> {
        let mut buffer = Vec::with_capacity(64 * 1024);
        let mut offsets = Vec::new();
        let mut reader = Reader::from_reader(BufReader::with_capacity(256 * 1024, xml));
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
                    let start = buffer.len() as u32;
                    let bytes = current_text.as_bytes();
                    buffer.extend_from_slice(bytes);
                    offsets.push((start, bytes.len() as u32));
                    current_text.clear();
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
        Ok(SharedStrings {
            buffer: Buffer::from_vec(buffer),
            offsets,
        })
    }

    fn read_styles<R: Read + Seek>(
        archive: &mut ZipArchive<R>,
    ) -> Result<(Vec<u32>, HashSet<u32>)> {
        let file = match archive.by_name("xl/styles.xml") {
            Ok(f) => f,
            Err(_) => return Ok((Vec::new(), HashSet::new())),
        };
        let mut reader = Reader::from_reader(BufReader::with_capacity(64 * 1024, file));
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
                    if let Some(id_str) = get_attribute(&e, b"numFmtId")? {
                        if let Ok(id) = id_str.parse::<u32>() {
                            if let Some(code) = get_attribute(&e, b"formatCode")? {
                                if is_date_format_code(&code) {
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
                    let num_fmt_id = get_attribute(&e, b"numFmtId")?
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
}

/// Find the first occurrence of `needle` in `haystack` using byte-window comparison.
#[inline]
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}
