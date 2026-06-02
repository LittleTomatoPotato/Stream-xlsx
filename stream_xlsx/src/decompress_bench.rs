#[cfg(test)]
mod tests {
    use std::io::{BufReader, Read};
    use std::iter;
    use std::path::Path;
    use std::time::Instant;

    const TEST_FILE: &str = "../test_100w_60c.xlsx";
    const BUF_SIZE: usize = 256 * 1024;

    /// 列出 ZIP 内所有条目，帮助确认测试对象。
    #[test]
    fn list_zip_entries() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("跳过：{} 不存在", path.display());
            return;
        }
        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = zip::ZipArchive::new(reader).unwrap();

        eprintln!("{:<40} {:>12} {:>12} {:>8}", "Name", "Compressed", "Uncompressed", "Ratio");
        eprintln!("{}", iter::repeat('-').take(80).collect::<String>());
        let mut total_compressed = 0u64;
        let mut total_uncompressed = 0u64;
        let entries: Vec<(String, u64, u64)> = (0..archive.len())
            .map(|i| {
                let f = archive.by_index(i).unwrap();
                (f.name().to_string(), f.compressed_size(), f.size())
            })
            .collect();
        for (name, c, u) in entries {
            total_compressed += c;
            total_uncompressed += u;
            eprintln!(
                "{:<40} {:>12} {:>12} {:>7.2}x",
                name,
                human_size(c),
                human_size(u),
                u as f64 / c.max(1) as f64
            );
        }
        eprintln!("{}", iter::repeat('-').take(80).collect::<String>());
        eprintln!(
            "{:<40} {:>12} {:>12} {:>7.2}x",
            "TOTAL", human_size(total_compressed), human_size(total_uncompressed),
            total_uncompressed as f64 / total_compressed.max(1) as f64
        );
    }

    /// 解压指定条目 N 次，测量吞吐率。
    fn bench_entry(archive: &mut zip::ZipArchive<BufReader<std::fs::File>>, idx: usize, iterations: usize) -> f64 {
        // 预热
        {
            let mut zf = archive.by_index(idx).unwrap();
            let mut discard = vec![0u8; BUF_SIZE];
            while zf.read(&mut discard).unwrap() > 0 {}
        }

        let uncompressed = {
            let f = archive.by_index(idx).unwrap();
            f.size()
        };
        let mut total = 0u64;
        let start = Instant::now();

        for _ in 0..iterations {
            let mut zf = archive.by_index(idx).unwrap();
            let mut buf = vec![0u8; BUF_SIZE];
            loop {
                let n = zf.read(&mut buf).unwrap();
                if n == 0 { break; }
                total += n as u64;
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        assert_eq!(total, uncompressed * iterations as u64);
        (total as f64 / 1024.0 / 1024.0) / elapsed
    }

    /// 解压所有条目各一次，测量总吞吐率。
    fn bench_all(archive: &mut zip::ZipArchive<BufReader<std::fs::File>>) -> f64 {
        let mut total_uncompressed = 0u64;

        // 预热
        for i in 0..archive.len() {
            let mut zf = archive.by_index(i).unwrap();
            total_uncompressed += zf.size();
            let mut discard = vec![0u8; BUF_SIZE];
            while zf.read(&mut discard).unwrap() > 0 {}
        }

        let start = Instant::now();
        for i in 0..archive.len() {
            let mut zf = archive.by_index(i).unwrap();
            let mut buf = vec![0u8; BUF_SIZE];
            loop {
                let n = zf.read(&mut buf).unwrap();
                if n == 0 { break; }
            }
        }
        let elapsed = start.elapsed().as_secs_f64();

        (total_uncompressed as f64 / 1024.0 / 1024.0) / elapsed
    }

    /// 测量 `XlsxWorkbook::open` + `init` 中各阶段耗时。
    #[test]
    fn profile_workbook_init() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("跳过：{} 不存在", path.display());
            return;
        }

        let sep: String = iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("profile_workbook_init (1 iteration, includes XML parsing)");
        eprintln!("{}", sep);

        // Phase 1: open (reads rels + workbook.xml only)
        let t0 = Instant::now();
        let wb = crate::workbook::XlsxWorkbook::open(path).unwrap();
        let open_elapsed = t0.elapsed().as_secs_f64();
        eprintln!("Phase 1  open()          : {:.3}s (rels + workbook.xml)", open_elapsed);

        // Phase 2: init (reads sharedStrings.xml + styles.xml)
        let t1 = Instant::now();
        wb.init().unwrap();
        let init_elapsed = t1.elapsed().as_secs_f64();
        eprintln!("Phase 2  init()          : {:.3}s (sharedStrings + styles)", init_elapsed);

        eprintln!("{}", sep);
        eprintln!("Total workbook setup     : {:.3}s", open_elapsed + init_elapsed);
        eprintln!("{}", sep);
    }

    /// 纯字节扫描解析 sharedStrings.xml（不用 quick-xml），验证理论上限。
    #[test]
    fn parse_shared_strings_raw() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("跳过：{} 不存在", path.display());
            return;
        }

        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = zip::ZipArchive::new(reader).unwrap();
        let mut file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let mut xml = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut xml).unwrap();

        let sep: String = iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("纯字节扫描解析 sharedStrings.xml");
        eprintln!("{}", sep);

        // 方案 A: 简单扫描 <si><t>...</t></si>
        let t0 = Instant::now();
        let mut buffer = Vec::with_capacity(xml.len() / 4);
        let mut offsets: Vec<(u32, u32)> = Vec::with_capacity(xml.len() / 32);

        let mut i = 0;
        while i < xml.len() {
            // 找 <si>
            if let Some(pos) = find_subsequence(&xml[i..], b"<si><t>") {
                i += pos + 7;
                // 找 </t></si>
                if let Some(end) = find_subsequence(&xml[i..], b"</t></si>") {
                    let text = &xml[i..i + end];
                    let start = buffer.len() as u32;
                    buffer.extend_from_slice(text);
                    offsets.push((start, end as u32));
                    i += end + 9;
                    continue;
                }
            }
            break;
        }

        let elapsed = t0.elapsed().as_secs_f64();
        eprintln!(
            "简单扫描: 解析 {} 个字符串, 耗时 {:.3}s, 吞吐 {:.2} MB/s",
            offsets.len(),
            elapsed,
            (xml.len() as f64 / 1024.0 / 1024.0) / elapsed
        );

        // 方案 B: 处理 CDATA 和 XML 实体
        let t1 = Instant::now();
        let mut buffer2 = Vec::with_capacity(xml.len() / 4);
        let mut offsets2: Vec<(u32, u32)> = Vec::with_capacity(xml.len() / 32);

        let mut i = 0;
        while i < xml.len() {
            // 找 <si>
            if let Some(pos) = find_subsequence(&xml[i..], b"<si>") {
                i += pos + 4;
                // 找 <t> 或 <t ...>
                if let Some(t_start) = find_subsequence(&xml[i..], b"<t>") {
                    i += t_start + 3;
                    // 找 </t>
                    if let Some(t_end) = find_subsequence(&xml[i..], b"</t>") {
                        let text = &xml[i..i + t_end];
                        let start = buffer2.len() as u32;
                        buffer2.extend_from_slice(text);
                        offsets2.push((start, t_end as u32));
                        i += t_end + 4;
                        // 跳过到 </si>
                        if let Some(si_end) = find_subsequence(&xml[i..], b"</si>") {
                            i += si_end + 5;
                        }
                        continue;
                    }
                }
            }
            break;
        }

        let elapsed2 = t1.elapsed().as_secs_f64();
        eprintln!(
            "增强扫描: 解析 {} 个字符串, 耗时 {:.3}s, 吞吐 {:.2} MB/s",
            offsets2.len(),
            elapsed2,
            (xml.len() as f64 / 1024.0 / 1024.0) / elapsed2
        );

        eprintln!("{}", sep);

        // 验证结果一致性
        if offsets.len() > 0 && offsets.len() == offsets2.len() {
            eprintln!("✅ 两种扫描方案解析的字符串数量一致");
        } else {
            eprintln!("⚠️  字符串数量不一致: 简单={}, 增强={}", offsets.len(), offsets2.len());
        }
    }

    /// 对比 quick-xml vs 字节扫描的正确性和性能。
    #[test]
    fn compare_shared_strings_parsers() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("跳过：{} 不存在", path.display());
            return;
        }

        let sep: String = iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("对比 quick-xml vs 字节扫描");
        eprintln!("{}", sep);

        // 1. quick-xml 解析
        let t0 = Instant::now();
        let wb = crate::workbook::XlsxWorkbook::open(path).unwrap();
        wb.init().unwrap();
        let quick_xml_time = t0.elapsed().as_secs_f64();
        let strings = wb.strings().unwrap();
        let quick_count = strings.offsets.len();
        eprintln!("quick-xml : {} 个字符串, 耗时 {:.3}s", quick_count, quick_xml_time);

        // 2. 字节扫描解析
        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = zip::ZipArchive::new(reader).unwrap();
        let mut file = archive.by_name("xl/sharedStrings.xml").unwrap();
        let mut xml = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut xml).unwrap();

        let t1 = Instant::now();
        let (scan_buffer, scan_offsets) = parse_shared_strings_fast(&xml);
        let scan_time = t1.elapsed().as_secs_f64();
        let scan_count = scan_offsets.len();
        eprintln!("字节扫描  : {} 个字符串, 耗时 {:.3}s", scan_count, scan_time);

        eprintln!("{}", sep);

        // 3. 验证一致性
        if quick_count != scan_count {
            eprintln!("❌ 字符串数量不一致: quick-xml={}, 扫描={}", quick_count, scan_count);
            return;
        }

        let mut mismatches = 0;
        let max_check = quick_count.min(10000); // 抽样验证前 10000 个
        for i in 0..max_check {
            let (q_start, q_len) = strings.offsets[i];
            let (s_start, s_len) = scan_offsets[i];
            let q_text = &strings.buffer.as_slice()[q_start as usize..(q_start + q_len) as usize];
            let s_text = &scan_buffer[s_start as usize..(s_start + s_len) as usize];
            if q_text != s_text {
                mismatches += 1;
                if mismatches <= 3 {
                    eprintln!(
                        "❌ 第 {} 个字符串不匹配:\n  quick-xml: {:?}\n  扫描    : {:?}",
                        i,
                        String::from_utf8_lossy(q_text),
                        String::from_utf8_lossy(s_text)
                    );
                }
            }
        }

        if mismatches == 0 {
            eprintln!("✅ 前 {} 个字符串内容完全一致", max_check);
        } else {
            eprintln!("❌ {} / {} 个字符串内容不匹配", mismatches, max_check);
        }

        if scan_count > 0 {
            eprintln!(
                "📈 加速比: {:.1}x",
                quick_xml_time / scan_time
            );
        }
        eprintln!("{}", sep);
    }

    /// 快速字节扫描解析 sharedStrings.xml。
    fn parse_shared_strings_fast(xml: &[u8]) -> (Vec<u8>, Vec<(u32, u32)>) {
        let mut buffer = Vec::with_capacity(xml.len() / 4);
        let mut offsets = Vec::with_capacity(xml.len() / 32);

        let mut i = 0;
        while i < xml.len() {
            // 找 <si>
            let si_pos = match find_subsequence(&xml[i..], b"<si>") {
                Some(p) => p,
                None => break,
            };
            i += si_pos + 4;

            // 找 <t> 或 <t ...>
            let t_pos = match find_subsequence(&xml[i..], b"<t") {
                Some(p) => p,
                None => { i += 1; continue; }
            };
            i += t_pos;

            // 跳过 <t> 或 <t ...> 到 >
            while i < xml.len() && xml[i] != b'>' {
                i += 1;
            }
            i += 1; // skip >

            // 找 </t>
            let t_end = match find_subsequence(&xml[i..], b"</t>") {
                Some(p) => p,
                None => break,
            };

            let text = &xml[i..i + t_end];
            let start = buffer.len() as u32;
            buffer.extend_from_slice(text);
            offsets.push((start, t_end as u32));

            i += t_end + 4;

            // 跳过到 </si>
            let si_end = match find_subsequence(&xml[i..], b"</si>") {
                Some(p) => p,
                None => break,
            };
            i += si_end + 5;
        }

        (buffer, offsets)
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    /// 综合解压基准：分别测 sheet1.xml、sharedStrings.xml、全量解压。
    #[test]
    fn decompress_raw() {
        let path = Path::new(TEST_FILE);
        if !path.exists() {
            eprintln!("跳过基准测试：{} 不存在", path.display());
            return;
        }

        let file = std::fs::File::open(path).unwrap();
        let reader = BufReader::new(file);
        let mut archive = zip::ZipArchive::new(reader).unwrap();

        // 按名称查找索引（提前收集，避免持有 borrow）
        let entries: Vec<(usize, String, u64, u64)> = (0..archive.len())
            .map(|i| {
                let f = archive.by_index(i).unwrap();
                (i, f.name().to_string(), f.compressed_size(), f.size())
            })
            .collect();

        let mut sheet1_idx = None;
        let mut ss_idx = None;
        for (i, name, _, _) in &entries {
            if name.ends_with("sheet1.xml") {
                sheet1_idx = Some(*i);
            } else if name == "xl/sharedStrings.xml" {
                ss_idx = Some(*i);
            }
        }

        let sep: String = iter::repeat('=').take(60).collect();
        eprintln!("\n{}", sep);
        eprintln!("综合解压基准");
        eprintln!("{}", sep);

        if let Some(idx) = sheet1_idx {
            let (_, _, c, u) = entries.iter().find(|(i, _, _, _)| *i == idx).unwrap();
            let thr = bench_entry(&mut archive, idx, 5);
            eprintln!(
                "sheet1.xml   | 压缩 {:>10} → 解压 {:>10} | {:.2}x | 吞吐 {:.2} MB/s",
                human_size(*c), human_size(*u), *u as f64 / (*c).max(1) as f64, thr
            );
        }

        if let Some(idx) = ss_idx {
            let (_, _, c, u) = entries.iter().find(|(i, _, _, _)| *i == idx).unwrap();
            let thr = bench_entry(&mut archive, idx, 5);
            eprintln!(
                "sharedStrings| 压缩 {:>10} → 解压 {:>10} | {:.2}x | 吞吐 {:.2} MB/s",
                human_size(*c), human_size(*u), *u as f64 / (*c).max(1) as f64, thr
            );
        }

        let thr = bench_all(&mut archive);
        eprintln!("全量解压     | 所有条目                          | 吞吐 {:.2} MB/s", thr);
        eprintln!("{}", sep);
    }

    fn human_size(bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
        let mut size = bytes as f64;
        let mut unit_idx = 0;
        while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
            size /= 1024.0;
            unit_idx += 1;
        }
        format!("{:.2} {}", size, UNITS[unit_idx])
    }
}
