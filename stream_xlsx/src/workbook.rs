use crate::utils::*;
use anyhow::{Context, Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use zip::ZipArchive;

/// 工作簿级共享数据，解析 sheet 列表立即完成，strings/styles 惰性加载。
#[derive(Debug)]
pub struct XlsxWorkbook {
    path: PathBuf,
    strings: OnceLock<Arc<Vec<Box<str>>>>,
    cell_xfs: OnceLock<Arc<Vec<u32>>>,
    custom_date_numfmts: OnceLock<Arc<HashSet<u32>>>,
    sheets: OrderdSheets,
}

impl XlsxWorkbook {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
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

        let strings = Arc::new(Self::read_shared_strings(&mut archive)?);
        let (cell_xfs, custom_date_numfmts) = Self::read_styles(&mut archive)?;

        let _ = self.strings.set(strings);
        let _ = self.cell_xfs.set(Arc::new(cell_xfs));
        let _ = self.custom_date_numfmts.set(Arc::new(custom_date_numfmts));
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn strings(&self) -> Option<&Arc<Vec<Box<str>>>> {
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

    fn read_shared_strings<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Result<Vec<Box<str>>> {
        let file = match archive.by_name("xl/sharedStrings.xml") {
            Ok(f) => f,
            Err(_) => return Ok(Vec::new()),
        };
        let mut strings = Vec::new();
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
                    strings.push(std::mem::take(&mut current_text).into_boxed_str());
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
