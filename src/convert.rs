use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::Error;
use crate::parse::Session;
use crate::pdfout::Document;

pub struct Report {
    pub pages: usize,
    pub warnings: Vec<String>,
}

pub fn convert_file(
    src: &Path,
    dst: &Path,
    stop: &AtomicBool,
    mut progress: impl FnMut(u32, u32),
) -> Result<Report, Error> {
    let file = File::open(src).map_err(|e| Error::msg(format!("无法打开 {}: {e}", src.display())))?;
    let (bytes, report) = convert_reader(file, stop, &mut progress)?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::msg(format!("无法创建目录: {e}")))?;
    }
    let tmp = dst.with_extension("pdf.partial");
    std::fs::write(&tmp, bytes).map_err(|e| Error::msg(format!("无法写入: {e}")))?;
    if dst.exists() {
        std::fs::remove_file(dst).map_err(|e| Error::msg(format!("无法覆盖 {}: {e}", dst.display())))?;
    }
    std::fs::rename(&tmp, dst).map_err(|e| Error::msg(format!("无法保存 PDF: {e}")))?;
    Ok(report)
}

pub fn convert_reader<R: Read + Seek>(
    reader: R,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<(Vec<u8>, Report), Error> {
    let mut session = Session::open(reader)?;
    let parts = session.page_parts()?;
    let total = parts.len() as u32;
    if total == 0 {
        return Err(Error::msg("XPS 里没有页面"));
    }
    let mut doc = Document::new();
    for (index, part) in parts.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Stopped);
        }
        progress(index as u32 + 1, total);
        let page = session.parse_page(part)?;
        let mut extra = Vec::new();
        doc.add_page(session.package_mut(), &page, &mut extra)?;
        for warning in extra {
            session.push_warning(warning);
        }
    }
    let warnings = session.warnings().to_vec();
    let pages = parts.len();
    Ok((doc.finish(), Report { pages, warnings }))
}

pub struct Handle {
    stop: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub struct Job {
    pub id: u64,
    pub path: PathBuf,
    pub dest: PathBuf,
}

pub enum Event {
    Progress { id: u64, page: u32, total: u32 },
    Done { id: u64, path: PathBuf, detail: String },
    Failed { id: u64, err: String },
    Finished { stopped: bool },
}

pub fn start(jobs: Vec<Job>) -> (Handle, async_channel::Receiver<Event>) {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let (tx, rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        let mut stopped = false;
        for job in jobs {
            if flag.load(Ordering::Relaxed) {
                stopped = true;
                break;
            }
            let id = job.id;
            let result = convert_file(&job.path, &job.dest, &flag, |page, total| {
                let _ = tx.send_blocking(Event::Progress { id, page, total });
            });
            match result {
                Ok(report) => {
                    let warn = if report.warnings.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", report.warnings.join("; "))
                    };
                    let _ = tx.send_blocking(Event::Done {
                        id,
                        path: job.dest,
                        detail: format!("{} 页{warn}", report.pages),
                    });
                }
                Err(Error::Stopped) => {
                    stopped = true;
                    let _ = tx.send_blocking(Event::Failed { id, err: "已停止".into() });
                    break;
                }
                Err(e) => {
                    let _ = tx.send_blocking(Event::Failed { id, err: e.to_string() });
                }
            }
        }
        let _ = tx.send_blocking(Event::Finished { stopped });
    });
    (Handle { stop }, rx)
}

pub fn is_xps_path(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => ext.eq_ignore_ascii_case("xps") || ext.eq_ignore_ascii_case("oxps"),
        None => false,
    }
}

pub fn collect_inputs(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        walk(path, 0, &mut out);
    }
    out
}

fn walk(path: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        let mut kids: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        kids.sort();
        for kid in kids {
            walk(&kid, depth + 1, out);
        }
        return;
    }
    if meta.is_file() && is_xps_path(path) {
        out.push(path.to_path_buf());
    }
}

pub fn dest_pdf(out_dir: &Path, src: &Path, used: &mut Vec<String>) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("output");
    let mut name = format!("{stem}.pdf");
    let mut n = 2u32;
    while used.iter().any(|u| u.eq_ignore_ascii_case(&name)) {
        name = format!("{stem} ({n}).pdf");
        n += 1;
    }
    used.push(name.clone());
    out_dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn xps(files: &[(&str, &str)]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            for (name, body) in files {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    #[test]
    fn black_rect_becomes_pdf() {
        let bytes = xps(&[
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.microsoft.com/xps/2005/06/fixedrepresentation" Target="/FixedDocumentSequence.fdseq"/>
</Relationships>"#,
            ),
            (
                "FixedDocumentSequence.fdseq",
                r#"<FixedDocumentSequence xmlns="http://schemas.microsoft.com/xps/2005/06">
  <DocumentReference Source="Documents/1/FixedDocument.fdoc"/>
</FixedDocumentSequence>"#,
            ),
            (
                "Documents/1/FixedDocument.fdoc",
                r#"<FixedDocument xmlns="http://schemas.microsoft.com/xps/2005/06">
  <PageContent Source="Pages/1.fpage"/>
</FixedDocument>"#,
            ),
            (
                "Documents/1/Pages/1.fpage",
                r##"<FixedPage xmlns="http://schemas.microsoft.com/xps/2005/06" Width="96" Height="96">
  <Path Fill="#FF000000" Data="F1 M 10,10 L 80,10 L 80,80 L 10,80 Z"/>
  <Path Stroke="#FFFF0000" StrokeThickness="2" Data="M 0,0 L 96,96"/>
</FixedPage>"##,
            ),
        ]);
        let stop = AtomicBool::new(false);
        let (pdf, report) = convert_reader(Cursor::new(bytes), &stop, &mut |_, _| {}).unwrap();
        assert_eq!(report.pages, 1);
        assert!(pdf.starts_with(b"%PDF"));
        assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
        assert!(pdf.len() > 400);
    }

    #[test]
    fn glyphs_use_embedded_font() {
        let candidates = [
            r"C:\Windows\Fonts\arial.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ];
        let Some(font_path) = candidates.iter().map(std::path::Path::new).find(|p| p.is_file()) else {
            return;
        };
        let font = std::fs::read(font_path).unwrap();
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            let files = [
                (
                    "_rels/.rels",
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.microsoft.com/xps/2005/06/fixedrepresentation" Target="/FixedDocumentSequence.fdseq"/>
</Relationships>"#,
                ),
                (
                    "FixedDocumentSequence.fdseq",
                    r#"<FixedDocumentSequence xmlns="http://schemas.microsoft.com/xps/2005/06">
  <DocumentReference Source="Documents/1/FixedDocument.fdoc"/>
</FixedDocumentSequence>"#,
                ),
                (
                    "Documents/1/FixedDocument.fdoc",
                    r#"<FixedDocument xmlns="http://schemas.microsoft.com/xps/2005/06">
  <PageContent Source="Pages/1.fpage"/>
</FixedDocument>"#,
                ),
                (
                    "Documents/1/Pages/1.fpage",
                    r##"<FixedPage xmlns="http://schemas.microsoft.com/xps/2005/06" Width="200" Height="200">
  <Glyphs Fill="#FF000000" FontUri="../Resources/Fonts/arial.ttf" FontRenderingEmSize="32" OriginX="20" OriginY="80" UnicodeString="Hi"/>
</FixedPage>"##,
                ),
            ];
            for (name, body) in files {
                zip.start_file(name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.start_file("Documents/1/Resources/Fonts/arial.ttf", opts).unwrap();
            zip.write_all(&font).unwrap();
            zip.finish().unwrap();
        }
        let stop = AtomicBool::new(false);
        let (pdf, report) = convert_reader(Cursor::new(cursor.into_inner()), &stop, &mut |_, _| {}).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.pages, 1);
        assert!(pdf.starts_with(b"%PDF"));
        assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
        let text = inflate_first_stream(&pdf);
        assert!(text.contains("/ActualText (Hi)"), "{text}");
        assert!(text.contains(" l\n") || text.contains(" c\n"), "{text}");
        assert!(text.contains("\nf\n") || text.ends_with("\nf"), "{text}");
    }

    fn inflate_first_stream(pdf: &[u8]) -> String {
        let length_at = pdf.windows(8).position(|w| w == b"/Length ").unwrap() + 8;
        let mut length = 0usize;
        for b in &pdf[length_at..] {
            if b.is_ascii_digit() {
                length = length * 10 + (*b - b'0') as usize;
            } else {
                break;
            }
        }
        let start = pdf.windows(7).position(|w| w == b"stream\n").unwrap() + 7;
        let inflated = miniz_oxide::inflate::decompress_to_vec_zlib(&pdf[start..start + length]).unwrap();
        String::from_utf8(inflated).unwrap()
    }
}
