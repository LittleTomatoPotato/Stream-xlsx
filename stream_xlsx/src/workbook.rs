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

        if self.fast_shared_strings {
            // Concurrent fast path: decompress in a background thread while the
            // main thread parses.  Falls back to serial fast path on error.
            match Self::parse_shared_strings_concurrent(&self.path) {
                Ok(strings) => {
                    let _ = self.strings.set(Arc::new(strings));
                }
                Err(_e) => {
                    let file = std::fs::File::open(&self.path)?;
                    let reader = BufReader::new(file);
                    let mut archive = ZipArchive::new(reader)?;
                    let strings = Arc::new(Self::read_shared_strings(&mut archive, true)?);
                    let _ = self.strings.set(strings);
                }
            }
        } else {
            let file = std::fs::File::open(&self.path)?;
            let reader = BufReader::new(file);
            let mut archive = ZipArchive::new(reader)?;
            let strings = Arc::new(Self::read_shared_strings(&mut archive, false)?);
            let _ = self.strings.set(strings);
        }

        // read_styles is fast enough to stay serial; open a fresh archive.
        let file = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader)?;
        let (cell_xfs, custom_date_numfmts) = Self::read_styles(&mut archive)?;
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
        let mut file = match archive.by_name("xl/sharedStrings.xml") {
            Ok(f) => f,
            Err(_) => {
                return Ok(SharedStrings {
                    buffer: Buffer::from_vec(Vec::new()),
                    offsets: Vec::new(),
                });
            }
        };

        if fast {
            // Fast path: pre-allocate exact capacity to avoid Vec reallocations
            // (which were causing the ~5GB peak), then scan in-place.
            let uncompressed_size = file.size() as usize;
            let mut xml = if uncompressed_size > 0 {
                Vec::with_capacity(uncompressed_size)
            } else {
                // Data descriptor: size unknown until read. Use compressed size × 5 as heuristic.
                let estimated = (file.compressed_size() as usize).saturating_mul(5);
                Vec::with_capacity(estimated.max(64 * 1024))
            };
            file.read_to_end(&mut xml)?;
            match Self::parse_shared_strings_fast(xml) {
                Ok(result) => return Ok(result),
                Err((xml, _e)) => {
                    // Fallback for rich text / CDATA / entities
                    Self::parse_shared_strings_xml(std::io::Cursor::new(xml))
                }
            }
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

    /// Concurrent fast path: background thread decompresses sharedStrings.xml
    /// while the main thread parses chunks via a bounded channel.
    /// Overlaps I/O and CPU; total time drops to ~max(decompress, parse).
    fn parse_shared_strings_concurrent(path: &Path) -> Result<SharedStrings> {
        // Probe uncompressed size so we can pre-allocate the text buffer.
        let uncompressed_size = {
            let file = std::fs::File::open(path)?;
            let reader = BufReader::new(file);
            let mut archive = ZipArchive::new(reader)?;
            let file = archive.by_name("xl/sharedStrings.xml")?;
            file.size() as usize
        };

        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        let path2 = path.to_path_buf();

        let decompress_handle = std::thread::spawn(move || -> Result<()> {
            let file = std::fs::File::open(&path2)?;
            let reader = BufReader::new(file);
            let mut archive = ZipArchive::new(reader)?;
            let mut file = archive.by_name("xl/sharedStrings.xml")?;
            let mut chunk = vec![0u8; 8 * 1024 * 1024];
            loop {
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                if tx.send(chunk[..n].to_vec()).is_err() {
                    break;
                }
            }
            Ok(())
        });

        let mut buffer = Vec::with_capacity(uncompressed_size / 2);
        let mut offsets = Vec::with_capacity(uncompressed_size / 32);
        let mut accumulate = Vec::new();

        while let Ok(data) = rx.recv() {
            accumulate.extend_from_slice(&data);
            let processed =
                process_complete_sis_concurrent(&accumulate, &mut buffer, &mut offsets, false)?;
            if processed > 0 {
                accumulate.drain(..processed);
            }
        }

        if !accumulate.is_empty() {
            let _processed =
                process_complete_sis_concurrent(&accumulate, &mut buffer, &mut offsets, true)?;
        }

        decompress_handle
            .join()
            .map_err(|e| anyhow!("Decompress thread panicked: {:?}", e))??;

        Ok(SharedStrings {
            buffer: Buffer::from_vec(buffer),
            offsets,
        })
    }

    /// Fast path: scan raw bytes for `<si><t>...</t></si>` patterns.
    /// In-place: reuses the decompressed XML buffer, eliminating the extra
    /// 2GB buffer allocation. With exact-capacity pre-allocation peak memory
    /// stays at ~2.4 GB (2 GB xml + 430 MB offsets) instead of ~5 GB.
    fn parse_shared_strings_fast(
        mut xml: Vec<u8>,
    ) -> Result<SharedStrings, (Vec<u8>, anyhow::Error)> {
        let mut offsets = Vec::with_capacity(xml.len() / 32);
        let mut write_pos = 0;
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
                None => return Err((xml, anyhow!("No </si> found"))),
            };
            let si_content = &xml[i..i + si_end];

            // Check for rich text (<r>) or CDATA — not supported in fast path
            if si_content.windows(3).any(|w| w == b"<r>") {
                return Err((xml, anyhow!("Rich text <r> not supported in fast path")));
            }
            if si_content.windows(9).any(|w| w == b"<![CDATA[") {
                return Err((xml, anyhow!("CDATA not supported in fast path")));
            }

            // Find <t> or <t ...> within this <si>
            let t_pos = match find_subsequence(si_content, b"<t") {
                Some(p) => p,
                None => {
                    // Empty string: <si></si> or <si><t/></si>
                    offsets.push((write_pos as u32, 0));
                    i += si_end + 5;
                    continue;
                }
            };

            let mut t_start = t_pos + 2;
            // Skip attributes until '>'
            while t_start < si_content.len() && si_content[t_start] != b'>' {
                if si_content[t_start] == b'&' {
                    return Err((xml, anyhow!("XML entity in <t> attribute not supported")));
                }
                t_start += 1;
            }
            if t_start >= si_content.len() {
                return Err((xml, anyhow!("Unclosed <t> tag")));
            }
            // Detect self-closing tag <t/> or <t attr="val"/>
            if t_start > t_pos + 2 && si_content[t_start - 1] == b'/' {
                offsets.push((write_pos as u32, 0));
                i += si_end + 5;
                continue;
            }
            t_start += 1; // skip '>'

            // Find </t>
            let t_end = match find_subsequence(&si_content[t_start..], b"</t>") {
                Some(p) => p,
                None => return Err((xml, anyhow!("No </t> found"))),
            };

            let text_len = t_end;

            // XML entity check in text content
            if si_content[t_start..t_start + text_len].contains(&b'&') {
                return Err((
                    xml,
                    anyhow!("XML entity in text not supported in fast path"),
                ));
            }

            // In-place memmove: copy text forward to overwrite XML tags.
            // Safe because write_pos always lags behind the source (i + t_start),
            // since each <si> block contains ~16 bytes of tag overhead.
            unsafe {
                std::ptr::copy(
                    xml.as_ptr().add(i + t_start),
                    xml.as_mut_ptr().add(write_pos),
                    text_len,
                );
            }

            offsets.push((write_pos as u32, text_len as u32));
            write_pos += text_len;
            i += si_end + 5;
        }

        xml.truncate(write_pos);
        xml.shrink_to_fit();

        Ok(SharedStrings {
            buffer: Buffer::from_vec(xml),
            offsets,
        })
    }

    /// Fallback: full quick-xml parser for complex sharedStrings.
    fn parse_shared_strings_xml<R: Read>(reader: R) -> Result<SharedStrings> {
        let mut buffer = Vec::with_capacity(64 * 1024);
        let mut offsets = Vec::new();
        let mut reader = Reader::from_reader(BufReader::with_capacity(256 * 1024, reader));
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

impl XlsxWorkbook {
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
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::time::Instant;

    const TEST_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../test_100w_60c.xlsx");

    /// 精确测量 init() 每个子阶段的耗时，找出 4.2s 与 2.5s 的差距。
    #[test]
    fn profile_init_breakdown() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("跳过：{} 不存在", path.display());
            return;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("init() 阶段拆分 — 找出理论 2.5s 与实际 4.2s 的差距");
        eprintln!("{}", sep);

        // 阶段 1: 打开文件 + ZipArchive::new
        let t0 = Instant::now();
        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).unwrap();
        let t_open = t0.elapsed().as_secs_f64();
        eprintln!("① 打开文件 + ZipArchive::new : {:.3}s", t_open);

        // 阶段 2: by_name sharedStrings.xml
        let t1 = Instant::now();
        let mut file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let t_by_name = t1.elapsed().as_secs_f64();
        eprintln!("② by_name(sharedStrings)      : {:.3}s", t_by_name);

        // 阶段 3: 预分配
        let t2 = Instant::now();
        let size = file.size() as usize;
        let mut xml = Vec::with_capacity(size);
        let t_alloc = t2.elapsed().as_secs_f64();
        eprintln!(
            "③ Vec::with_capacity({:.0}MB) : {:.3}s",
            size as f64 / 1024.0 / 1024.0,
            t_alloc
        );

        // 阶段 4: read_to_end 解压
        let t3 = Instant::now();
        file.read_to_end(&mut xml).unwrap();
        drop(file); // release mutable borrow on archive
        let t_read = t3.elapsed().as_secs_f64();
        eprintln!(
            "④ read_to_end (解压)          : {:.3}s  [{:.0} MB/s]",
            t_read,
            xml.len() as f64 / 1024.0 / 1024.0 / t_read
        );

        // 阶段 5: parse_shared_strings_fast
        let t4 = Instant::now();
        let ss = XlsxWorkbook::parse_shared_strings_fast(xml).unwrap();
        let t_parse = t4.elapsed().as_secs_f64();
        eprintln!("⑤ parse_shared_strings_fast   : {:.3}s", t_parse);
        eprintln!("   → 解析 {} 个字符串", ss.offsets.len());

        // 阶段 6: by_name + read_styles
        let t5 = Instant::now();
        let _styles = XlsxWorkbook::read_styles(&mut archive).unwrap();
        let t_styles = t5.elapsed().as_secs_f64();
        eprintln!("⑥ read_styles                 : {:.3}s", t_styles);

        let total = t_open + t_by_name + t_alloc + t_read + t_parse + t_styles;
        eprintln!("{}", sep);
        eprintln!("总计: {:.3}s", total);
        eprintln!("理论下限 (④+⑤): {:.3}s", t_read + t_parse);
        eprintln!("差距: {:.3}s", total - t_read - t_parse);
        eprintln!("{}", sep);
    }

    /// 测量 parse_shared_strings_fast 各子步骤的耗时，定位 2.3s 的瓶颈。
    #[test]
    fn profile_parse_fast_breakdown() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            return;
        }

        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).unwrap();
        let mut file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let mut xml = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut xml).unwrap();
        drop(file);

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("parse_shared_strings_fast 子步骤拆分");
        eprintln!("{}", sep);

        // 方案 A: 完整版（当前实现）
        {
            let xml2 = xml.clone();
            let t0 = Instant::now();
            let _ss = XlsxWorkbook::parse_shared_strings_fast(xml2).unwrap();
            eprintln!(
                "A. 完整版（含检查）           : {:.3}s",
                t0.elapsed().as_secs_f64()
            );
        }

        // 方案 B: 仅 find_subsequence（4 次搜索）
        {
            let t0 = Instant::now();
            let mut count = 0usize;
            let mut i = 0;
            while i < xml.len() {
                if let Some(p) = find_subsequence(&xml[i..], b"<si>") {
                    i += p + 4;
                    if let Some(e) = find_subsequence(&xml[i..], b"</si>") {
                        let si_content = &xml[i..i + e];
                        if let Some(tp) = find_subsequence(si_content, b"<t") {
                            let mut ts = tp + 2;
                            while ts < si_content.len() && si_content[ts] != b'>' {
                                ts += 1;
                            }
                            ts += 1;
                            if let Some(_te) = find_subsequence(&si_content[ts..], b"</t>") {
                                count += 1;
                                i += e + 5;
                                continue;
                            }
                        }
                        count += 1;
                        i += e + 5;
                        continue;
                    }
                }
                break;
            }
            eprintln!(
                "B. 仅 find_subsequence（4次） : {:.3}s  [count={}]",
                t0.elapsed().as_secs_f64(),
                count
            );
        }

        // 方案 C: 仅 find_subsequence（2 次搜索，像 raw 测试）
        {
            let t0 = Instant::now();
            let mut count = 0usize;
            let mut i = 0;
            while i < xml.len() {
                if let Some(p) = find_subsequence(&xml[i..], b"<si><t>") {
                    i += p + 7;
                    if let Some(e) = find_subsequence(&xml[i..], b"</t></si>") {
                        count += 1;
                        i += e + 9;
                        continue;
                    }
                }
                break;
            }
            eprintln!(
                "C. 仅 find_subsequence（2次） : {:.3}s  [count={}]",
                t0.elapsed().as_secs_f64(),
                count
            );
        }

        // 方案 D: 4 次搜索 + <r>/CDATA 检查
        {
            let t0 = Instant::now();
            let mut count = 0usize;
            let mut i = 0;
            while i < xml.len() {
                if let Some(p) = find_subsequence(&xml[i..], b"<si>") {
                    i += p + 4;
                    if let Some(e) = find_subsequence(&xml[i..], b"</si>") {
                        let si_content = &xml[i..i + e];
                        let _has_r = si_content.windows(3).any(|w| w == b"<r>");
                        let _has_cdata = si_content.windows(9).any(|w| w == b"<![CDATA[");
                        if let Some(tp) = find_subsequence(si_content, b"<t") {
                            let mut ts = tp + 2;
                            while ts < si_content.len() && si_content[ts] != b'>' {
                                ts += 1;
                            }
                            ts += 1;
                            if let Some(_te) = find_subsequence(&si_content[ts..], b"</t>") {
                                count += 1;
                                i += e + 5;
                                continue;
                            }
                        }
                        count += 1;
                        i += e + 5;
                        continue;
                    }
                }
                break;
            }
            eprintln!(
                "D. 4次搜索 + <r>/CDATA 检查  : {:.3}s  [count={}]",
                t0.elapsed().as_secs_f64(),
                count
            );
        }

        // 方案 E: 4 次搜索 + <r>/CDATA 检查 + ptr::copy
        {
            let mut xml2 = xml.clone();
            let t0 = Instant::now();
            let mut write_pos = 0;
            let mut count = 0usize;
            let mut i = 0;
            while i < xml2.len() {
                if let Some(p) = find_subsequence(&xml2[i..], b"<si>") {
                    i += p + 4;
                    if let Some(e) = find_subsequence(&xml2[i..], b"</si>") {
                        let si_content = &xml2[i..i + e];
                        let _has_r = si_content.windows(3).any(|w| w == b"<r>");
                        let _has_cdata = si_content.windows(9).any(|w| w == b"<![CDATA[");
                        if let Some(tp) = find_subsequence(si_content, b"<t") {
                            let mut ts = tp + 2;
                            while ts < si_content.len() && si_content[ts] != b'>' {
                                ts += 1;
                            }
                            if ts > tp + 2 && si_content[ts - 1] == b'/' {
                                count += 1;
                                i += e + 5;
                                continue;
                            }
                            ts += 1;
                            if let Some(te) = find_subsequence(&si_content[ts..], b"</t>") {
                                unsafe {
                                    std::ptr::copy(
                                        xml2.as_ptr().add(i + ts),
                                        xml2.as_mut_ptr().add(write_pos),
                                        te,
                                    );
                                }
                                write_pos += te;
                                count += 1;
                                i += e + 5;
                                continue;
                            }
                        }
                        count += 1;
                        i += e + 5;
                        continue;
                    }
                }
                break;
            }
            xml2.truncate(write_pos);
            eprintln!(
                "E. 4次搜索 + 检查 + ptr::copy : {:.3}s  [count={}]",
                t0.elapsed().as_secs_f64(),
                count
            );
        }

        eprintln!("{}", sep);
    }

    /// 验证 offsets.push 的开销是否是瓶颈。
    #[test]
    fn profile_offsets_push() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            return;
        }

        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).unwrap();
        let mut file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let mut xml = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut xml).unwrap();
        drop(file);

        // 先解析出所有字符串的位置信息
        let mut positions: Vec<(usize, usize)> = Vec::with_capacity(xml.len() / 32);
        let mut i = 0;
        while i < xml.len() {
            if let Some(p) = find_subsequence(&xml[i..], b"<si>") {
                i += p + 4;
                if let Some(e) = find_subsequence(&xml[i..], b"</si>") {
                    let si_content = &xml[i..i + e];
                    if let Some(tp) = find_subsequence(si_content, b"<t") {
                        let mut ts = tp + 2;
                        while ts < si_content.len() && si_content[ts] != b'>' {
                            ts += 1;
                        }
                        if ts > tp + 2 && si_content[ts - 1] == b'/' {
                            positions.push((0, 0));
                            i += e + 5;
                            continue;
                        }
                        ts += 1;
                        if let Some(te) = find_subsequence(&si_content[ts..], b"</t>") {
                            positions.push((i + ts, te));
                            i += e + 5;
                            continue;
                        }
                    }
                    positions.push((0, 0));
                    i += e + 5;
                    continue;
                }
            }
            break;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("offsets.push 开销验证");
        eprintln!("{}", sep);
        eprintln!("positions.len() = {}", positions.len());

        // A: 直接 push 到 Vec<(u32, u32)>
        {
            let mut offsets: Vec<(u32, u32)> = Vec::with_capacity(positions.len());
            let t0 = Instant::now();
            for &(start, len) in &positions {
                offsets.push((start as u32, len as u32));
            }
            eprintln!(
                "A. push (u32,u32) × {}       : {:.3}s",
                positions.len(),
                t0.elapsed().as_secs_f64()
            );
        }

        // B: 同时做 ptr::copy + push
        {
            let mut xml2 = xml.clone();
            let mut offsets: Vec<(u32, u32)> = Vec::with_capacity(positions.len());
            let mut write_pos = 0;
            let t0 = Instant::now();
            for &(start, len) in &positions {
                unsafe {
                    std::ptr::copy(
                        xml2.as_ptr().add(start),
                        xml2.as_mut_ptr().add(write_pos),
                        len,
                    );
                }
                offsets.push((write_pos as u32, len as u32));
                write_pos += len;
            }
            xml2.truncate(write_pos);
            eprintln!(
                "B. ptr::copy + push × {}     : {:.3}s",
                positions.len(),
                t0.elapsed().as_secs_f64()
            );
        }

        // C: 用 extend_from_slice + push（像低内存模式）
        {
            let mut buffer = Vec::with_capacity(xml.len() / 4);
            let mut offsets: Vec<(u32, u32)> = Vec::with_capacity(positions.len());
            let t0 = Instant::now();
            for &(start, len) in &positions {
                let start_idx = buffer.len() as u32;
                buffer.extend_from_slice(&xml[start..start + len]);
                offsets.push((start_idx, len as u32));
            }
            eprintln!(
                "C. extend + push × {}        : {:.3}s",
                positions.len(),
                t0.elapsed().as_secs_f64()
            );
        }

        eprintln!("{}", sep);
    }

    /// 验证 contains(&b'&') 的开销。
    #[test]
    fn profile_contains_ampersand() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            return;
        }

        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).unwrap();
        let mut file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let mut xml = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut xml).unwrap();
        drop(file);

        // 先解析出所有文本片段
        let mut texts: Vec<&[u8]> = Vec::with_capacity(xml.len() / 32);
        let mut i = 0;
        while i < xml.len() {
            if let Some(p) = find_subsequence(&xml[i..], b"<si>") {
                i += p + 4;
                if let Some(e) = find_subsequence(&xml[i..], b"</si>") {
                    let si_content = &xml[i..i + e];
                    if let Some(tp) = find_subsequence(si_content, b"<t") {
                        let mut ts = tp + 2;
                        while ts < si_content.len() && si_content[ts] != b'>' {
                            ts += 1;
                        }
                        if ts > tp + 2 && si_content[ts - 1] == b'/' {
                            texts.push(b"");
                            i += e + 5;
                            continue;
                        }
                        ts += 1;
                        if let Some(te) = find_subsequence(&si_content[ts..], b"</t>") {
                            texts.push(&si_content[ts..ts + te]);
                            i += e + 5;
                            continue;
                        }
                    }
                    texts.push(b"");
                    i += e + 5;
                    continue;
                }
            }
            break;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("contains(&b'&') 开销验证");
        eprintln!("{}", sep);
        eprintln!("texts.len() = {}", texts.len());

        // A: 只做 contains
        {
            let mut count = 0usize;
            let t0 = Instant::now();
            for text in &texts {
                if text.contains(&b'&') {
                    count += 1;
                }
            }
            eprintln!(
                "A. contains(&b'&') × {}      : {:.3}s  [hits={}]",
                texts.len(),
                t0.elapsed().as_secs_f64(),
                count
            );
        }

        // B: 扫描文本中的每个字节（模拟最坏情况）
        {
            let mut count = 0usize;
            let t0 = Instant::now();
            for text in &texts {
                for &b in *text {
                    if b == b'&' {
                        count += 1;
                    }
                }
            }
            eprintln!(
                "B. 逐字节扫描 × {}           : {:.3}s  [hits={}]",
                texts.len(),
                t0.elapsed().as_secs_f64(),
                count
            );
        }

        eprintln!("{}", sep);
    }

    /// 对比 zip::ZipFile 的 read_to_end vs read 循环 vs 预期理论速度。
    #[test]
    fn profile_decompress_speed() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            return;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("解压速度对比 — sharedStrings.xml vs sheet1.xml");
        eprintln!("{}", sep);

        for (name, entry_name) in [
            ("sharedStrings.xml", "xl/sharedStrings.xml"),
            ("sheet1.xml", "xl/worksheets/sheet1.xml"),
        ] {
            let file = std::fs::File::open(path).unwrap();
            let reader = BufReader::new(file);
            let mut archive = ZipArchive::new(reader).unwrap();
            let f = archive.by_name(entry_name).unwrap();
            let compressed = f.compressed_size();
            let uncompressed = f.size();
            drop(f);
            drop(archive);

            // 方案 A: read_to_end
            {
                let file = std::fs::File::open(path).unwrap();
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader).unwrap();
                let mut f = archive.by_name(entry_name).unwrap();
                let mut xml = Vec::with_capacity(uncompressed as usize);
                let t0 = Instant::now();
                f.read_to_end(&mut xml).unwrap();
                let dt = t0.elapsed().as_secs_f64();
                eprintln!(
                    "{} | read_to_end: {:.3}s  [{:.0} MB/s uncompressed, {:.0} MB/s compressed]",
                    name,
                    dt,
                    uncompressed as f64 / 1024.0 / 1024.0 / dt,
                    compressed as f64 / 1024.0 / 1024.0 / dt
                );
            }

            // 方案 B: read loop (1MB buf)
            {
                let file = std::fs::File::open(path).unwrap();
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader).unwrap();
                let mut f = archive.by_name(entry_name).unwrap();
                let mut buf = vec![0u8; 1024 * 1024];
                let mut total = 0usize;
                let t0 = Instant::now();
                loop {
                    let n = f.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                let dt = t0.elapsed().as_secs_f64();
                eprintln!(
                    "{} | read loop  : {:.3}s  [{:.0} MB/s uncompressed, {:.0} MB/s compressed]",
                    name,
                    dt,
                    total as f64 / 1024.0 / 1024.0 / dt,
                    compressed as f64 / 1024.0 / 1024.0 / dt
                );
            }

            // 方案 C: read loop (8MB buf)
            {
                let file = std::fs::File::open(path).unwrap();
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader).unwrap();
                let mut f = archive.by_name(entry_name).unwrap();
                let mut buf = vec![0u8; 8 * 1024 * 1024];
                let mut total = 0usize;
                let t0 = Instant::now();
                loop {
                    let n = f.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                let dt = t0.elapsed().as_secs_f64();
                eprintln!(
                    "{} | read loop8M: {:.3}s  [{:.0} MB/s uncompressed, {:.0} MB/s compressed]",
                    name,
                    dt,
                    total as f64 / 1024.0 / 1024.0 / dt,
                    compressed as f64 / 1024.0 / 1024.0 / dt
                );
            }
        }

        eprintln!("{}", sep);
    }

    /// 并发解压 + 解析 sharedStrings.xml 的可行性验证。
    /// 线程 A 边解压边发送 chunk，线程 B 边接收边解析。
    #[test]
    fn profile_concurrent_decompress_parse() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            return;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("并发解压+解析可行性验证");
        eprintln!("{}", sep);

        // 先获取文件大小用于预分配
        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = ZipArchive::new(reader).unwrap();
        let file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let uncompressed_size = file.size() as usize;
        drop(file);
        drop(archive);

        // 方案 A: 串行（当前实现）
        {
            let file = std::fs::File::open(path).unwrap();
            let reader = BufReader::new(file);
            let mut archive = ZipArchive::new(reader).unwrap();
            let mut f = archive.by_name("xl/sharedStrings.xml").unwrap();
            let mut xml = Vec::with_capacity(uncompressed_size);
            let t0 = Instant::now();
            f.read_to_end(&mut xml).unwrap();
            let _ss = XlsxWorkbook::parse_shared_strings_fast(xml).unwrap();
            let dt = t0.elapsed().as_secs_f64();
            eprintln!("A. 串行 (read_to_end + fast)  : {:.3}s", dt);
        }

        // 方案 B: 并发 — channel + extend_from_slice（预分配1GB）
        {
            let t0 = Instant::now();
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);

            let path2 = path.to_path_buf();
            let decompress_handle = std::thread::spawn(move || {
                let file = std::fs::File::open(&path2).unwrap();
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader).unwrap();
                let mut f = archive.by_name("xl/sharedStrings.xml").unwrap();
                let mut chunk = vec![0u8; 8 * 1024 * 1024];
                loop {
                    let n = f.read(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });

            let mut buffer = Vec::with_capacity(uncompressed_size / 2);
            let mut offsets = Vec::with_capacity(uncompressed_size / 32);
            let mut accumulate = Vec::new();

            while let Ok(data) = rx.recv() {
                accumulate.extend_from_slice(&data);
                let processed =
                    process_complete_sis_concurrent(&accumulate, &mut buffer, &mut offsets, false)
                        .unwrap();
                if processed > 0 {
                    accumulate.drain(..processed);
                }
            }

            if !accumulate.is_empty() {
                let _processed =
                    process_complete_sis_concurrent(&accumulate, &mut buffer, &mut offsets, true)
                        .unwrap();
            }

            decompress_handle.join().unwrap();
            let dt = t0.elapsed().as_secs_f64();
            eprintln!(
                "B. 并发 (channel + extend 1GB) : {:.3}s  [offsets={}]",
                dt,
                offsets.len()
            );
        }

        // 方案 C: 并发 — 预分配更大 buffer（2GB）
        {
            let t0 = Instant::now();
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);

            let path2 = path.to_path_buf();
            let decompress_handle = std::thread::spawn(move || {
                let file = std::fs::File::open(&path2).unwrap();
                let reader = BufReader::new(file);
                let mut archive = ZipArchive::new(reader).unwrap();
                let mut f = archive.by_name("xl/sharedStrings.xml").unwrap();
                let mut chunk = vec![0u8; 8 * 1024 * 1024];
                loop {
                    let n = f.read(&mut chunk).unwrap();
                    if n == 0 {
                        break;
                    }
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });

            let mut buffer = Vec::with_capacity(uncompressed_size);
            let mut offsets = Vec::with_capacity(uncompressed_size / 32);
            let mut accumulate = Vec::new();

            while let Ok(data) = rx.recv() {
                accumulate.extend_from_slice(&data);
                let processed =
                    process_complete_sis_concurrent(&accumulate, &mut buffer, &mut offsets, false)
                        .unwrap();
                if processed > 0 {
                    accumulate.drain(..processed);
                }
            }

            if !accumulate.is_empty() {
                let _processed =
                    process_complete_sis_concurrent(&accumulate, &mut buffer, &mut offsets, true)
                        .unwrap();
            }

            decompress_handle.join().unwrap();
            let dt = t0.elapsed().as_secs_f64();
            eprintln!(
                "C. 并发 (预分配2GB buffer)      : {:.3}s  [offsets={}]",
                dt,
                offsets.len()
            );
        }

        eprintln!("{}", sep);
    }
}

/// Streaming byte-scanner for `<si><t>...</t></si>` used by the concurrent
/// fast path.  Performs the same safety checks as `parse_shared_strings_fast`
/// so that malformed input can be rejected and the caller may fall back.
fn process_complete_sis_concurrent(
    data: &[u8],
    buffer: &mut Vec<u8>,
    offsets: &mut Vec<(u32, u32)>,
    at_eof: bool,
) -> Result<usize> {
    let mut i = 0;
    while i < data.len() {
        let si_pos = match find_subsequence(&data[i..], b"<si>") {
            Some(p) => p,
            None => {
                if at_eof {
                    return Ok(data.len());
                }
                break;
            }
        };
        let si_content_start = i + si_pos + 4;
        let si_end = match find_subsequence(&data[si_content_start..], b"</si>") {
            Some(p) => p,
            None => {
                if at_eof {
                    return Err(anyhow!("Incomplete shared string at EOF"));
                }
                break;
            }
        };
        let si_content = &data[si_content_start..si_content_start + si_end];

        // Rich text / CDATA — fall back to full XML parser
        if si_content.windows(3).any(|w| w == b"<r>") {
            return Err(anyhow!("Rich text <r> not supported in fast path"));
        }
        if si_content.windows(9).any(|w| w == b"<![CDATA[") {
            return Err(anyhow!("CDATA not supported in fast path"));
        }

        let t_pos = match find_subsequence(si_content, b"<t") {
            Some(p) => p,
            None => {
                offsets.push((buffer.len() as u32, 0));
                i = si_content_start + si_end + 5;
                continue;
            }
        };

        let mut t_start = t_pos + 2;
        while t_start < si_content.len() && si_content[t_start] != b'>' {
            if si_content[t_start] == b'&' {
                return Err(anyhow!("XML entity in <t> attribute not supported"));
            }
            t_start += 1;
        }
        if t_start >= si_content.len() {
            return Err(anyhow!("Unclosed <t> tag"));
        }
        if t_start > t_pos + 2 && si_content[t_start - 1] == b'/' {
            offsets.push((buffer.len() as u32, 0));
            i = si_content_start + si_end + 5;
            continue;
        }
        t_start += 1;

        let t_end = match find_subsequence(&si_content[t_start..], b"</t>") {
            Some(p) => p,
            None => {
                if at_eof {
                    return Err(anyhow!("No </t> found at EOF"));
                }
                break;
            }
        };

        let text = &si_content[t_start..t_start + t_end];
        if text.contains(&b'&') {
            return Err(anyhow!("XML entity in text not supported in fast path"));
        }

        let start = buffer.len() as u32;
        buffer.extend_from_slice(text);
        offsets.push((start, t_end as u32));

        i = si_content_start + si_end + 5;
    }
    Ok(i)
}
