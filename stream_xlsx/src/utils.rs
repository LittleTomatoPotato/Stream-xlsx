use crate::excel_types::{CellErrorType, Data, Dimensions};
use anyhow::{Result, anyhow};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct OrderdSheets {
    order: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl OrderdSheets {
    pub fn new() -> Self {
        Self {
            order: Vec::new(),
            index: HashMap::new(),
        }
    }
    pub fn insert(&mut self, name: String, path: String) {
        let idx = self.order.len();
        self.index.insert(name.clone(), idx);
        self.order.push((name, path));
    }
    pub fn get_by_name(&self, name: &str) -> Option<&String> {
        self.index
            .get(name)
            .and_then(|&idx| self.order.get(idx))
            .map(|(_, path)| path)
    }
    pub fn get_by_idx(&self, idx: usize) -> Option<&String> {
        self.order.get(idx).map(|(_, path)| path)
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn names(&self) -> Vec<&str> {
        self.order.iter().map(|(name, _)| name.as_str()).collect()
    }
}

pub fn is_date_format_code(code: &str) -> bool {
    let code_lower = code.to_lowercase();
    let date_keywords = ['y', 'm', 'd', 'h', 's'];
    date_keywords.iter().any(|&k| code_lower.contains(k))
        && !code_lower.contains("0")
        && !code_lower.contains("#")
}

pub fn is_date_numfmt(num_fmt_id: u32, custom_date_numfmts: &HashSet<u32>) -> bool {
    matches!(num_fmt_id, 14..=22 | 45..=47) || custom_date_numfmts.contains(&num_fmt_id)
}

pub fn get_attribute(e: &BytesStart, key: &[u8]) -> Result<Option<String>> {
    for attr in e.attributes() {
        let attr = attr?;
        if attr.key.as_ref() == key {
            return Ok(Some(String::from_utf8_lossy(&attr.value).into_owned()));
        }
    }
    Ok(None)
}

pub fn parse_a1(s: &[u8]) -> Result<(u32, u32)> {
    let mut i = 0;
    let mut col = 0u32;
    while i < s.len() && s[i].is_ascii_alphabetic() {
        col = col * 26 + ((s[i].to_ascii_uppercase() - b'A' + 1) as u32);
        i += 1;
    }
    let mut row = 0u32;
    while i < s.len() && s[i].is_ascii_digit() {
        row = row * 10 + ((s[i] - b'0') as u32);
        i += 1;
    }
    Ok((row.saturating_sub(1), col.saturating_sub(1)))
}

pub fn parse_dimension(ref_attr: &[u8]) -> Result<Dimensions> {
    let parts: Vec<&[u8]> = ref_attr.split(|&b| b == b':').collect();
    if parts.len() == 1 {
        let (row, col) = parse_a1(parts[0])?;
        Ok(Dimensions {
            start: (row, col),
            end: (row, col),
        })
    } else if parts.len() == 2 {
        let (start_row, start_col) = parse_a1(parts[0])?;
        let (end_row, end_col) = parse_a1(parts[1])?;
        Ok(Dimensions {
            start: (start_row, start_col),
            end: (end_row, end_col),
        })
    } else {
        Err(anyhow!(
            "Invalid dimension: {}",
            String::from_utf8_lossy(ref_attr)
        ))
    }
}

pub fn parse_cell_pos(e: &BytesStart, default_row: u32, default_col: u32) -> Result<(u32, u32)> {
    if let Some(r) = get_attribute(e, b"r")? {
        parse_a1(r.as_bytes())
    } else {
        Ok((default_row, default_col))
    }
}

pub fn read_text_content<R: std::io::BufRead>(
    xml: &mut Reader<R>,
    scratch_buf: &mut Vec<u8>,
    end_tag: &[u8],
) -> Result<String> {
    let mut text = String::new();
    loop {
        scratch_buf.clear();
        match xml.read_event_into(scratch_buf) {
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

pub fn read_inline_str<R: std::io::BufRead>(
    xml: &mut Reader<R>,
    cell_buf: &mut Vec<u8>,
    scratch_buf: &mut Vec<u8>,
) -> Result<String> {
    let mut text = String::new();
    loop {
        cell_buf.clear();
        match xml.read_event_into(cell_buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"t" => {
                text.push_str(&read_text_content(xml, scratch_buf, b"t")?);
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"is" => break,
            Ok(Event::Eof) => return Err(anyhow!("Unexpected EOF in <is>")),
            Err(e) => return Err(anyhow!("XML error in <is>: {}", e)),
            _ => {}
        }
    }
    Ok(text)
}

pub fn parse_raw_value(text: &str, t_attr: Option<&str>) -> Result<Data> {
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
