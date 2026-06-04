use crate::excel_types::{Cell, Data, Dimensions};
use crate::utils::*;
use crate::workbook::SharedStrings;
use anyhow::{Result, anyhow};
use bytes::{Bytes, BytesMut};
use crossbeam_channel::bounded;
use polars::datatypes::PlSmallStr;
use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;

/// Fast mode 运行时配置
#[derive(Clone, Debug)]
pub struct FastConfig {
    /// 并发解析线程数
    pub parallelism: usize,
    /// 每个 chunk 包含的 cell 数，批量处理以减少管道往返
    pub chunk_size: usize,
    /// 管道容量倍数：实际容量 = parallelism * mul + 1
    pub queue_cap_mul: usize,
    /// 应用层读取缓冲区大小（字节）
    pub temp_size: usize,
    /// BufReader 缓冲区大小（字节）
    pub buf_size: usize,
}

impl FastConfig {
    pub fn queue_cap(&self) -> usize {
        self.parallelism.saturating_mul(self.queue_cap_mul).saturating_add(1)
    }

    /// 运行时默认值：parallelism = min(8, cores / 2)
    pub fn with_defaults() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);
        Self {
            parallelism: (cores / 2).min(8),
            chunk_size: 1000,
            queue_cap_mul: 1,
            temp_size: 1024 * 1024,
            buf_size: 1024 * 1024,
        }
    }
}

impl Default for FastConfig {
    fn default() -> Self {
        Self::with_defaults()
    }
}

// ------------------------------------------------------------------
// SheetFastReader: 对外 API
// ------------------------------------------------------------------

pub struct SheetFastReader {
    result_rx: crossbeam_channel::Receiver<Vec<(usize, Cell<Data>)>>,
    next_expected: usize,
    total_cells: usize,
    buffer: HashMap<usize, Cell<Data>>,
    pending: Vec<(usize, Cell<Data>)>,
    pending_idx: usize,
    _handles: Vec<std::thread::JoinHandle<()>>,
    strings: Arc<SharedStrings>,
    dimensions: Dimensions,
}

impl SheetFastReader {
    /// Open a sheet and start background parsing (streaming: unzip + scan on the fly).
    pub fn new(
        path: &Path,
        sheet_name: Option<&str>,
        sheet_idx: Option<usize>,
        config: Option<&FastConfig>,
    ) -> Result<Self> {
        let default_cfg = FastConfig::default();
        let config = config.unwrap_or(&default_cfg);
        let workbook = crate::workbook::XlsxWorkbook::open_fast(path)?;
        workbook.init()?;
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

        // 1. 快速读取 XML 头部解析 dimensions（不需要完整解压）
        let dimensions = {
            let file = std::fs::File::open(path)?;
            let reader = BufReader::with_capacity(config.buf_size, file);
            let mut archive = zip::ZipArchive::new(reader)?;
            let mut f = archive.by_name(&sheet_path)?;
            let mut head = vec![0u8; 8192];
            let n = f.read(&mut head)?;
            parse_dimensions(&head[..n]).unwrap_or_default()
        };
        let strings = Arc::clone(workbook.strings().unwrap());

        let queue_cap = config.queue_cap();
        let (task_tx, task_rx) = bounded(queue_cap);
        let (result_tx, result_rx) = bounded(queue_cap);
        let mut handles = Vec::new();

        let parallelism = config.parallelism;
        let chunk_size = config.chunk_size;
        let temp_size = config.temp_size;
        let buf_size = config.buf_size;

        // 2. 解压 + 扫描 + dispatch 线程（单线程，边读边扫）
        let path2 = path.to_path_buf();
        let sheet_path2 = sheet_path.clone();
        handles.push(std::thread::spawn(move || {
            let file = std::fs::File::open(&path2).unwrap();
            let reader = BufReader::with_capacity(buf_size, file);
            let mut archive = zip::ZipArchive::new(reader).unwrap();
            let mut f = archive.by_name(&sheet_path2).unwrap();
            let mut accumulate = BytesMut::with_capacity(8 * 1024 * 1024);
            let mut temp = vec![0u8; temp_size];
            let mut seq = 0usize;
            let mut chunk: Vec<(usize, Bytes)> = Vec::with_capacity(chunk_size);

            loop {
                let n = f.read(&mut temp).unwrap();
                if n == 0 {
                    break;
                }

                accumulate.extend_from_slice(&temp[..n]);
                let mut i = 0;

                while i + 2 < accumulate.len() {
                    if accumulate[i] == b'<'
                        && accumulate[i + 1] == b'c'
                        && (accumulate[i + 2] == b' ' || accumulate[i + 2] == b'>')
                    {
                        let c_start = i;
                        let mut j = i + 2;
                        let mut self_closing = false;

                        while j + 1 < accumulate.len() {
                            if accumulate[j] == b'/' && accumulate[j + 1] == b'>' {
                                self_closing = true;
                                j += 2;
                                break;
                            }
                            if accumulate[j] == b'>' {
                                j += 1;
                                break;
                            }
                            j += 1;
                        }

                        if self_closing {
                            if c_start > 0 {
                                let _garbage = accumulate.split_to(c_start);
                                j -= c_start;
                            }
                            let cell = accumulate.split_to(j).freeze();
                            chunk.push((seq, cell));
                            seq += 1;
                            i = 0;
                            if chunk.len() >= chunk_size {
                                let old = std::mem::replace(
                                    &mut chunk,
                                    Vec::with_capacity(chunk_size),
                                );
                                if task_tx.send(old).is_err() {
                                    return;
                                }
                            }
                            continue;
                        }

                        if j >= accumulate.len() {
                            break;
                        }

                        let mut found_close = false;
                        while j + 3 < accumulate.len() {
                            if accumulate[j] == b'<'
                                && accumulate[j + 1] == b'/'
                                && accumulate[j + 2] == b'c'
                                && accumulate[j + 3] == b'>'
                            {
                                j += 4;
                                found_close = true;
                                break;
                            }
                            j += 1;
                        }

                        if !found_close {
                            break;
                        }

                        if c_start > 0 {
                            let _garbage = accumulate.split_to(c_start);
                            j -= c_start;
                        }
                        let cell = accumulate.split_to(j).freeze();
                        chunk.push((seq, cell));
                        seq += 1;
                        i = 0;
                        if chunk.len() >= chunk_size {
                            let old =
                                std::mem::replace(&mut chunk, Vec::with_capacity(chunk_size));
                            if task_tx.send(old).is_err() {
                                return;
                            }
                        }
                    } else {
                        i += 1;
                    }
                }

                // break 后，丢弃已扫描的垃圾，保留不完整 cell
                if i > 0 && i < accumulate.len() {
                    let _garbage = accumulate.split_to(i);
                }
            }

            // 最终扫描：read 返回 0 后处理剩余数据
            let mut i = 0;
            while i + 2 < accumulate.len() {
                if accumulate[i] == b'<'
                    && accumulate[i + 1] == b'c'
                    && (accumulate[i + 2] == b' ' || accumulate[i + 2] == b'>')
                {
                    let c_start = i;
                    let mut j = i + 2;
                    let mut self_closing = false;

                    while j + 1 < accumulate.len() {
                        if accumulate[j] == b'/' && accumulate[j + 1] == b'>' {
                            self_closing = true;
                            j += 2;
                            break;
                        }
                        if accumulate[j] == b'>' {
                            j += 1;
                            break;
                        }
                        j += 1;
                    }

                    if self_closing {
                        if c_start > 0 {
                            let _garbage = accumulate.split_to(c_start);
                            j -= c_start;
                        }
                        let cell = accumulate.split_to(j).freeze();
                        chunk.push((seq, cell));
                        seq += 1;
                        i = 0;
                        if chunk.len() >= chunk_size {
                            let old = std::mem::replace(&mut chunk, Vec::with_capacity(chunk_size));
                            if task_tx.send(old).is_err() { return; }
                        }
                        continue;
                    }

                    if j >= accumulate.len() { break; }

                    let mut found_close = false;
                    while j + 3 < accumulate.len() {
                        if accumulate[j] == b'<'
                            && accumulate[j + 1] == b'/'
                            && accumulate[j + 2] == b'c'
                            && accumulate[j + 3] == b'>'
                        {
                            j += 4;
                            found_close = true;
                            break;
                        }
                        j += 1;
                    }

                    if !found_close { break; }

                    if c_start > 0 {
                        let _garbage = accumulate.split_to(c_start);
                        j -= c_start;
                    }
                    let cell = accumulate.split_to(j).freeze();
                    chunk.push((seq, cell));
                    seq += 1;
                    i = 0;
                    if chunk.len() >= chunk_size {
                        let old = std::mem::replace(&mut chunk, Vec::with_capacity(chunk_size));
                        if task_tx.send(old).is_err() { return; }
                    }
                } else {
                    i += 1;
                }
            }

            if !chunk.is_empty() {
                let _ = task_tx.send(chunk);
            }
        }));

        // 3. 工作线程（N 个）：解析 cell fragment
        for _ in 0..parallelism {
            let task_rx2 = task_rx.clone();
            let result_tx2 = result_tx.clone();
            handles.push(std::thread::spawn(move || {
                while let Ok(chunk) = task_rx2.recv() {
                    let mut batch = Vec::with_capacity(chunk.len());
                    for (seq, raw) in chunk {
                        let cell = parse_cell_fragment(&raw, seq)
                            .unwrap_or_else(|_| Cell::new((0, 0), Data::Empty));
                        batch.push((seq, cell));
                    }
                    if result_tx2.send(batch).is_err() {
                        break;
                    }
                }
            }));
        }
        drop(task_rx);
        drop(result_tx);

        Ok(Self {
            result_rx,
            next_expected: 0,
            total_cells: 0,
            buffer: HashMap::new(),
            pending: Vec::new(),
            pending_idx: 0,
            _handles: handles,
            strings,
            dimensions,
        })
    }

    pub fn dimensions(&self) -> Dimensions {
        self.dimensions
    }

    pub fn strings(&self) -> &Arc<SharedStrings> {
        &self.strings
    }

    pub fn total_cells(&self) -> usize {
        self.total_cells
    }

    /// Consume the next cell in strict ascending index order.
    pub fn next_cell(&mut self) -> Result<Option<Cell<Data>>> {
        if self.total_cells > 0 && self.next_expected >= self.total_cells {
            return Ok(None);
        }

        // 1) 优先从 pending batch 顺序消费
        if self.pending_idx < self.pending.len() {
            let (seq, _) = self.pending[self.pending_idx];
            if seq == self.next_expected {
                let cell = std::mem::replace(
                    &mut self.pending[self.pending_idx].1,
                    Cell::new((0, 0), Data::Empty),
                );
                self.pending_idx += 1;
                self.next_expected += 1;
                return Ok(Some(cell));
            }
            self.pending.clear();
            self.pending_idx = 0;
        }

        // 2) 检查排序缓冲
        if let Some(cell) = self.buffer.remove(&self.next_expected) {
            self.next_expected += 1;
            return Ok(Some(cell));
        }

        // 3) 从结果管道 recv batch
        while let Ok(mut batch) = self.result_rx.recv() {
            if batch.is_empty() {
                continue;
            }
            if batch[0].0 == self.next_expected {
                let cell = std::mem::replace(
                    &mut batch[0].1,
                    Cell::new((0, 0), Data::Empty),
                );
                self.pending = batch;
                self.pending_idx = 1;
                self.next_expected += 1;
                return Ok(Some(cell));
            }
            for (seq, cell) in batch {
                self.buffer.insert(seq, cell);
            }
            if let Some(cell) = self.buffer.remove(&self.next_expected) {
                self.next_expected += 1;
                return Ok(Some(cell));
            }
        }

        // 4) 管道已关闭，最后检查缓冲
        if let Some(cell) = self.buffer.remove(&self.next_expected) {
            self.next_expected += 1;
            return Ok(Some(cell));
        }

        Ok(None)
    }
}

// ------------------------------------------------------------------
// XML boundary scanner — pure byte scan, no attribute parsing
// ------------------------------------------------------------------

fn parse_dimensions(xml: &[u8]) -> Result<Dimensions> {
    let dim_open = find_subsequence(xml, b"<dimension ");
    let dim_empty = find_subsequence(xml, b"<dimension");
    let start = match (dim_open, dim_empty) {
        (Some(p), _) => p + 11, // len(b"<dimension ")
        (None, Some(p)) => p + 10, // len(b"<dimension")
        (None, None) => return Ok(Dimensions::default()),
    };
    let rest = &xml[start..];
    let tag_end = rest.iter().position(|&b| b == b'>').unwrap_or(rest.len());
    let attrs = &rest[..tag_end];
    if let Some(r_pos) = find_subsequence(attrs, b"ref=\"") {
        let r_start = r_pos + 5;
        if let Some(r_end) = attrs[r_start..].iter().position(|&b| b == b'"') {
            return crate::utils::parse_dimension(&attrs[r_start..r_start + r_end]);
        }
    }
    Ok(Dimensions::default())
}

// ------------------------------------------------------------------
// Cell fragment parser — zero-copy, no heap allocation in hot path
// ------------------------------------------------------------------

fn parse_cell_fragment(raw: &[u8], _idx: usize) -> Result<Cell<Data>> {
    let pos = extract_r_attr(raw)?;
    let t_attr = extract_t_attr(raw);

    if is_self_closing(raw) {
        return Ok(Cell::new(pos, Data::Empty));
    }

    if let Some(val) = extract_v_value(raw) {
        let data = match t_attr {
            Some(b"s") => {
                if let Ok(idx) = atoi_simd::parse::<usize, true, true>(val) {
                    Data::SharedStringRef(idx)
                } else {
                    let s = unsafe { std::str::from_utf8_unchecked(val) };
                    Data::String(PlSmallStr::from_str(s))
                }
            }
            Some(b"b") => Data::Bool(val != b"0"),
            Some(b"str") => {
                let s = unsafe { std::str::from_utf8_unchecked(val) };
                Data::String(PlSmallStr::from_str(s))
            }
            Some(b"e") => {
                let s = unsafe { std::str::from_utf8_unchecked(val) };
                Data::Error(
                    s.parse::<crate::excel_types::CellErrorType>()
                        .unwrap_or(crate::excel_types::CellErrorType::Value),
                )
            }
            _ => match atoi_simd::parse::<i64, true, true>(val) {
                Ok(v) => Data::Int(v),
                Err(_) => {
                    let s = unsafe { std::str::from_utf8_unchecked(val) };
                    match fast_float::parse::<f64, _>(s) {
                        Ok(v) => Data::Float(v),
                        Err(_) => Data::String(PlSmallStr::from_str(s)),
                    }
                }
            },
        };
        return Ok(Cell::new(pos, data));
    }

    if let Some(text) = extract_inline_text(raw) {
        let s = unsafe { std::str::from_utf8_unchecked(text) };
        return Ok(Cell::new(pos, Data::String(PlSmallStr::from_str(s))));
    }

    Ok(Cell::new(pos, Data::Empty))
}

fn extract_r_attr(raw: &[u8]) -> Result<(u32, u32)> {
    if let Some(start) = find_subsequence(raw, b"r=\"") {
        let start = start + 3;
        if let Some(end) = raw[start..].iter().position(|&b| b == b'"') {
            let a1 = &raw[start..start + end];
            return parse_a1(a1);
        }
    }
    Err(anyhow!("Missing r attribute"))
}

fn extract_t_attr(raw: &[u8]) -> Option<&[u8]> {
    if let Some(start) = find_subsequence(raw, b"t=\"") {
        let start = start + 3;
        if let Some(end) = raw[start..].iter().position(|&b| b == b'"') {
            return Some(&raw[start..start + end]);
        }
    }
    None
}

fn is_self_closing(raw: &[u8]) -> bool {
    if let Some(c_pos) = find_subsequence(raw, b"<c") {
        let rest = &raw[c_pos + 2..];
        for i in 0..rest.len().saturating_sub(1) {
            if rest[i] == b'/' && rest[i + 1] == b'>' {
                return true;
            }
            if rest[i] == b'>' {
                return false;
            }
        }
    }
    false
}

fn extract_v_value(raw: &[u8]) -> Option<&[u8]> {
    let v_open = find_subsequence(raw, b"<v>")?;
    let content_start = v_open + 3;
    let v_close = find_subsequence(&raw[content_start..], b"</v>")?;
    Some(&raw[content_start..content_start + v_close])
}

fn extract_inline_text(raw: &[u8]) -> Option<&[u8]> {
    let t_open = find_subsequence(raw, b"<t>")?;
    let content_start = t_open + 3;
    let t_close = find_subsequence(&raw[content_start..], b"</t>")?;
    Some(&raw[content_start..content_start + t_close])
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const TEST_FILE: &str = "../test_100w_60c.xlsx";

    #[test]
    fn profile_sheet_fast() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("Skip: {} not found", path.display());
            return;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("SheetFastReader prototype validation (batch CHUNK_SIZE={})", FastConfig::default().chunk_size);
        eprintln!("Baseline reference: 14.4s  [cells=59500954]");
        eprintln!("{}", sep);

        {
            let t_new = Instant::now();
            let mut reader = SheetFastReader::new(path, None, Some(0), &FastConfig::default()).unwrap();
            let new_elapsed = t_new.elapsed().as_secs_f64();
            eprintln!(
                "B. SheetFastReader::new()          : {:.3}s  (includes unzip+scan+dispatch start)",
                new_elapsed
            );

            let t_read = Instant::now();
            let mut count = 0usize;
            while let Ok(Some(_cell)) = reader.next_cell() {
                count += 1;
            }
            eprintln!(
                "B. SheetFastReader (concurrent)  : {:.3}s  [cells={}]",
                t_read.elapsed().as_secs_f64(),
                count
            );
        }

        eprintln!("{}", sep);
    }

    #[test]
    fn profile_sheet_fast_streaming() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("Skip: {} not found", path.display());
            return;
        }

        let sep: String = std::iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("SheetFastReader STREAMING (zero-copy)  [CHUNK_SIZE={}]", FastConfig::default().chunk_size);
        eprintln!("Baseline reference: 14.4s  [cells=59500954]");
        eprintln!("{}", sep);

        {
            let t_new = Instant::now();
            let mut reader = SheetFastReader::new(path, None, Some(0), &FastConfig::default()).unwrap();
            let new_elapsed = t_new.elapsed().as_secs_f64();
            eprintln!(
                "C. Streaming::new()                : {:.3}s  (includes unzip+scan+dispatch start)",
                new_elapsed
            );

            let t_read = Instant::now();
            let mut count = 0usize;
            while let Ok(Some(_cell)) = reader.next_cell() {
                count += 1;
            }
            eprintln!(
                "C. Streaming (concurrent)        : {:.3}s  [cells={}]",
                t_read.elapsed().as_secs_f64(),
                count
            );
        }

        eprintln!("{}", sep);
    }
}

