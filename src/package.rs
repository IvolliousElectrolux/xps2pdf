//! XPS / OXPS 是 OPC 压缩包. 部件名大小写在 macOS 上按字节比较, 这里做不区分大小写的回退.

use std::io::{Read, Seek};

use zip::ZipArchive;

use crate::error::Error;

pub struct Package<R: Read + Seek> {
    zip: ZipArchive<R>,
    names: Vec<String>,
}

impl<R: Read + Seek> Package<R> {
    pub fn open(reader: R) -> Result<Self, Error> {
        let mut zip = ZipArchive::new(reader).map_err(|e| Error::msg(format!("无法打开 XPS 压缩包: {e}")))?;
        let mut names = Vec::with_capacity(zip.len());
        for i in 0..zip.len() {
            let entry = zip.by_index(i).map_err(|e| Error::msg(format!("读取压缩包目录失败: {e}")))?;
            if entry.is_dir() {
                names.push(String::new());
                continue;
            }
            names.push(normalize_part(entry.name()));
        }
        Ok(Self { zip, names })
    }

    pub fn read_part(&mut self, name: &str) -> Result<Vec<u8>, Error> {
        let want = normalize_part(name);
        if let Some(bytes) = self.read_named(&want)? {
            return Ok(bytes);
        }
        let prefix = format!("{want}/[");
        let mut pieces: Vec<(u32, usize)> = Vec::new();
        for (idx, name) in self.names.iter().enumerate() {
            if name.starts_with(&prefix) && (name.ends_with(".piece") || name.ends_with(".last.piece")) {
                if let Some(ord) = piece_ord(name) {
                    pieces.push((ord, idx));
                }
            }
        }
        if pieces.is_empty() {
            return Err(Error::msg(format!("压缩包里没有部件 {want}")));
        }
        pieces.sort_by_key(|(ord, _)| *ord);
        let mut out = Vec::new();
        for (_, idx) in pieces {
            out.extend(self.read_index(idx)?);
        }
        Ok(out)
    }

    fn read_named(&mut self, want: &str) -> Result<Option<Vec<u8>>, Error> {
        if let Some(idx) = self.names.iter().position(|n| n == want) {
            return Ok(Some(self.read_index(idx)?));
        }
        let lower = want.to_ascii_lowercase();
        if let Some(idx) = self.names.iter().position(|n| n.to_ascii_lowercase() == lower) {
            return Ok(Some(self.read_index(idx)?));
        }
        Ok(None)
    }

    fn read_index(&mut self, idx: usize) -> Result<Vec<u8>, Error> {
        let mut entry = self.zip.by_index(idx).map_err(|e| Error::msg(format!("读取部件失败: {e}")))?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|e| Error::msg(format!("读取部件失败: {e}")))?;
        if buf.len() > 512 * 1024 * 1024 {
            return Err(Error::msg("单个部件超过 512 MB, 已中止"));
        }
        Ok(buf)
    }

    pub fn part_names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().filter(|n| !n.is_empty()).map(|s| s.as_str())
    }
}

pub fn normalize_part(raw: &str) -> String {
    let decoded = percent_decode(raw.trim());
    let slash = decoded.replace('\\', "/");
    let mut stack = Vec::new();
    for seg in slash.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            stack.pop();
            continue;
        }
        stack.push(seg);
    }
    stack.join("/")
}

pub fn resolve_uri(base_part: &str, uri: &str) -> String {
    let uri = uri.trim();
    let uri = uri.split(['?', '#']).next().unwrap_or(uri);
    if uri.starts_with('/') {
        return normalize_part(uri);
    }
    let base = normalize_part(base_part);
    let base_dir = match base.rfind('/') {
        Some(i) => &base[..i],
        None => "",
    };
    if base_dir.is_empty() {
        normalize_part(uri)
    } else {
        normalize_part(&format!("{base_dir}/{uri}"))
    }
}

fn piece_ord(name: &str) -> Option<u32> {
    let i = name.rfind('[')?;
    let rest = &name[i + 1..];
    let end = rest.find(']')?;
    rest[..end].parse().ok()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn xml_to_utf8(bytes: &[u8]) -> Result<String, Error> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(&bytes[3..]).into_owned());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return utf16_to_string(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return utf16_to_string(&bytes[2..], false);
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Ok(s.to_string());
    }
    let zeros = bytes.iter().take(64).filter(|b| **b == 0).count();
    if zeros > 8 {
        return utf16_to_string(bytes, true);
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn utf16_to_string(bytes: &[u8], le: bool) -> Result<String, Error> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
        .collect();
    String::from_utf16(&units).map_err(|_| Error::msg("XML 不是有效的 UTF-16"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_relative_font() {
        let p = resolve_uri("Documents/1/Pages/1.fpage", "../Resources/Fonts/A.odttf");
        assert_eq!(p, "Documents/1/Resources/Fonts/A.odttf");
    }
}
